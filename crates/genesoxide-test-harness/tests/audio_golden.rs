//! Audio golden comparison tests.
//!
//! Compares emulator audio output against a reference recording.
//! Place reference files in `tests/reference_audio/` (gitignored).
//!
//! Run with: cargo test -p genesoxide-test-harness --test audio_golden -- --nocapture

use genesoxide_core::api::{
    AudioEqStage, AudioOutputConfig, AudioOutputProfile, TimedPsgWrite, TimedYm2612Write,
};
use genesoxide_core::scheduler::MASTER_CLOCK_NTSC;
use genesoxide_core::{Command, GenesisCore};
use genesoxide_test_harness::vgm::{
    CoreAudioRenderer, Ym2612TrackedEvent, Ym2612TrackedEventKind,
    extract_ym2612_state_events_from_timed_writes, vgm_from_timed_ym2612_writes,
};
use genesoxide_test_harness::ymfm_reference::Ymfm2612Renderer;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const SONIC_ROM_PATH: &str =
    r"C:\Users\markm\AppData\Local\Temp\sonic_test\Sonic The Hedgehog (USA, Europe).md";

const REFERENCE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/reference_audio");
const GHZ_REFERENCE_MANIFEST_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/ghz_reference_manifest.json"
);

const SAMPLE_RATE: u32 = 44100;
const SIDE_DYNAMICS_ENV_WINDOW: usize = 1024;
const SIDE_DYNAMICS_MAX_LAG_BINS: usize = 3;

#[derive(Debug, Clone, Deserialize)]
struct GhzReferenceManifest {
    references: Vec<GhzReferenceManifestEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct GhzReferenceManifestEntry {
    filename: String,
    #[serde(default = "ghz_reference_manifest_enabled_default")]
    enabled: bool,
    #[serde(default = "ghz_reference_manifest_weight_default")]
    mono_weight: f32,
    #[serde(default = "ghz_reference_manifest_weight_default")]
    side_weight: f32,
    #[serde(rename = "source")]
    _source: Option<String>,
    #[serde(rename = "notes")]
    _notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct ResolvedGhzReference {
    path: PathBuf,
    mono_weight: f32,
    side_weight: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct GhzReferenceWeight {
    mono: f32,
    side: f32,
}

fn ghz_reference_manifest_enabled_default() -> bool {
    true
}

fn ghz_reference_manifest_weight_default() -> f32 {
    1.0
}

fn sanitize_ghz_reference_weight(weight: f32) -> f32 {
    if weight.is_finite() {
        weight.max(0.0)
    } else {
        1.0
    }
}

fn normalize_ghz_reference_manifest(manifest: GhzReferenceManifest) -> GhzReferenceManifest {
    GhzReferenceManifest {
        references: manifest
            .references
            .into_iter()
            .map(|mut entry| {
                entry.mono_weight = sanitize_ghz_reference_weight(entry.mono_weight);
                entry.side_weight = sanitize_ghz_reference_weight(entry.side_weight);
                entry
            })
            .collect(),
    }
}

fn parse_ghz_reference_manifest(json: &str) -> Option<GhzReferenceManifest> {
    serde_json::from_str::<GhzReferenceManifest>(json)
        .ok()
        .map(normalize_ghz_reference_manifest)
}

fn load_ghz_reference_manifest() -> Option<&'static GhzReferenceManifest> {
    static GHZ_REFERENCE_MANIFEST: OnceLock<Option<GhzReferenceManifest>> = OnceLock::new();
    GHZ_REFERENCE_MANIFEST
        .get_or_init(|| {
            std::fs::read_to_string(GHZ_REFERENCE_MANIFEST_PATH)
                .ok()
                .and_then(|json| parse_ghz_reference_manifest(&json))
        })
        .as_ref()
}

fn load_sonic() -> Option<Vec<u8>> {
    std::fs::read(SONIC_ROM_PATH).ok()
}

fn green_hill_has_live_ym_activity(core: &mut GenesisCore, frames: u32) -> bool {
    for _ in 0..frames {
        let before = core.ym2612_write_count();
        core.execute(Command::StepFrame);
        if core.ym2612_write_count() > before {
            return true;
        }
        core.clear_audio_buffer();
    }

    false
}

/// Load samples from a WAV file as f32 stereo pairs.
fn load_wav(path: &Path) -> Option<(u32, Vec<f32>)> {
    let reader = hound::WavReader::open(path).ok()?;
    let spec = reader.spec();
    let rate = spec.sample_rate;

    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .into_samples::<i32>()
                .filter_map(|s| s.ok())
                .map(|s| s as f32 / max)
                .collect()
        }
        hound::SampleFormat::Float => reader
            .into_samples::<f32>()
            .filter_map(|s| s.ok())
            .collect(),
    };

    Some((rate, samples))
}

/// Load samples from a FLAC file as f32 stereo pairs.
fn load_flac(path: &Path) -> Option<(u32, Vec<f32>)> {
    let mut reader = claxon::FlacReader::open(path).ok()?;
    let info = reader.streaminfo();
    let rate = info.sample_rate;
    let bps = info.bits_per_sample;
    let max = (1i64 << (bps - 1)) as f32;

    let samples: Vec<f32> = reader
        .samples()
        .filter_map(|s| s.ok())
        .map(|s| s as f32 / max)
        .collect();

    Some((rate, samples))
}

/// Load reference audio from any supported format.
fn load_reference(path: &Path) -> Option<(u32, Vec<f32>)> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("wav") => load_wav(path),
        Some("flac") => load_flac(path),
        _ => None,
    }
}

#[test]
#[ignore = "requires a local commercial Sonic ROM (see SONIC_ROM_PATH); run with -- --ignored"]
fn advance_to_green_hill_music_leaves_capture_near_live_ym_activity() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!(
                "[genesoxide][audio_golden] Sonic ROM not found at SONIC_ROM_PATH — \
                 place a commercial Sonic ROM there and run with -- --ignored to exercise this test."
            );
            return;
        }
    };

    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom));
    core.execute(Command::SetAudioSampleRate(SAMPLE_RATE));

    let extra_frames = advance_to_green_hill_music(&mut core);
    let saw_writes = green_hill_has_live_ym_activity(&mut core, 120);

    assert!(
        saw_writes,
        "expected live YM activity soon after GHZ music start, even after waiting {extra_frames} extra frames"
    );
}

/// Compute RMS energy of a slice of samples.
fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum_sq / samples.len() as f64).sqrt() as f32
}

fn stereo_lr_correlation(stereo: &[f32]) -> f32 {
    let left: Vec<f32> = stereo.iter().step_by(2).copied().collect();
    let right: Vec<f32> = stereo.iter().skip(1).step_by(2).copied().collect();
    cross_correlation(&left, &right)
}

fn stereo_side_ratio(stereo: &[f32]) -> f32 {
    let mut mid = Vec::with_capacity(stereo.len() / 2);
    let mut side = Vec::with_capacity(stereo.len() / 2);
    for frame in stereo.chunks_exact(2) {
        let left = frame[0];
        let right = frame[1];
        mid.push((left + right) * 0.5);
        side.push((left - right) * 0.5);
    }
    rms(&side) / rms(&mid).max(1e-9)
}

fn stereo_mid_side(stereo: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let mut mid = Vec::with_capacity(stereo.len() / 2);
    let mut side = Vec::with_capacity(stereo.len() / 2);
    for frame in stereo.chunks_exact(2) {
        let left = frame[0];
        let right = frame[1];
        mid.push((left + right) * 0.5);
        side.push((left - right) * 0.5);
    }
    (mid, side)
}

fn scale_side_channel(stereo: &[f32], side_scale: f32) -> Vec<f32> {
    let (mid, side) = stereo_mid_side(stereo);
    let mut output = Vec::with_capacity(stereo.len());
    for (&mid_sample, &side_sample) in mid.iter().zip(&side) {
        let scaled_side = side_sample * side_scale;
        output.push(mid_sample + scaled_side);
        output.push(mid_sample - scaled_side);
    }
    output
}

fn stereo_left_right(stereo: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let mut left = Vec::with_capacity(stereo.len() / 2);
    let mut right = Vec::with_capacity(stereo.len() / 2);
    for frame in stereo.chunks_exact(2) {
        left.push(frame[0]);
        right.push(frame[1]);
    }
    (left, right)
}

fn apply_side_transient_mix(stereo: &[f32], amount: f32) -> Vec<f32> {
    if stereo.is_empty() || amount.abs() <= f32::EPSILON {
        return stereo.to_vec();
    }

    let (mid, side) = stereo_mid_side(stereo);
    let mut output = Vec::with_capacity(stereo.len());
    let mut prev_side = 0.0f32;
    for (&mid_sample, &side_sample) in mid.iter().zip(&side) {
        let transient = side_sample - prev_side;
        let shaped_side = side_sample + transient * amount;
        output.push(mid_sample + shaped_side);
        output.push(mid_sample - shaped_side);
        prev_side = side_sample;
    }
    output
}

fn apply_side_delay_mix(stereo: &[f32], amount: f32, delay_samples: usize) -> Vec<f32> {
    if stereo.is_empty() || amount.abs() <= f32::EPSILON || delay_samples == 0 {
        return stereo.to_vec();
    }

    let (mid, side) = stereo_mid_side(stereo);
    let mut output = Vec::with_capacity(stereo.len());
    for (idx, (&mid_sample, &side_sample)) in mid.iter().zip(&side).enumerate() {
        let delayed = idx
            .checked_sub(delay_samples)
            .map_or(0.0, |delayed_idx| side[delayed_idx]);
        let shaped_side = side_sample + delayed * amount;
        output.push(mid_sample + shaped_side);
        output.push(mid_sample - shaped_side);
    }
    output
}

fn apply_side_triggered_persistence_mix(
    stereo: &[f32],
    trigger_samples: &[usize],
    amount: f32,
    decay_samples: usize,
) -> Vec<f32> {
    if stereo.is_empty()
        || trigger_samples.is_empty()
        || amount.abs() <= f32::EPSILON
        || decay_samples == 0
    {
        return stereo.to_vec();
    }

    let (mid, side) = stereo_mid_side(stereo);
    let mut output = Vec::with_capacity(stereo.len());
    let mut carry = 0.0f32;
    let decay = (-1.0f32 / decay_samples as f32).exp();
    let mut trigger_idx = 0usize;

    for (sample_idx, (&mid_sample, &side_sample)) in mid.iter().zip(&side).enumerate() {
        while let Some(&trigger_sample) = trigger_samples.get(trigger_idx) {
            if trigger_sample > sample_idx {
                break;
            }
            carry += side_sample * amount;
            trigger_idx += 1;
        }

        let shaped_side = side_sample + carry;
        output.push(mid_sample + shaped_side);
        output.push(mid_sample - shaped_side);
        carry *= decay;
    }

    output
}

fn apply_side_triggered_delayed_persistence_mix(
    stereo: &[f32],
    trigger_samples: &[usize],
    trigger_delay_samples: usize,
    amount: f32,
    decay_samples: usize,
) -> Vec<f32> {
    if trigger_delay_samples == 0 {
        return apply_side_triggered_persistence_mix(
            stereo,
            trigger_samples,
            amount,
            decay_samples,
        );
    }
    if stereo.is_empty()
        || trigger_samples.is_empty()
        || amount.abs() <= f32::EPSILON
        || decay_samples == 0
    {
        return stereo.to_vec();
    }

    let delayed_triggers: Vec<_> = trigger_samples
        .iter()
        .map(|sample| sample.saturating_add(trigger_delay_samples))
        .collect();
    apply_side_triggered_persistence_mix(stereo, &delayed_triggers, amount, decay_samples)
}

fn apply_side_triggered_transient_mix(
    stereo: &[f32],
    trigger_samples: &[usize],
    window_samples: usize,
    amount: f32,
) -> Vec<f32> {
    if stereo.is_empty()
        || trigger_samples.is_empty()
        || amount.abs() <= f32::EPSILON
        || window_samples == 0
    {
        return stereo.to_vec();
    }

    let (mid, side) = stereo_mid_side(stereo);
    let mut active = vec![false; side.len()];
    for &trigger in trigger_samples {
        let end = trigger.saturating_add(window_samples).min(active.len());
        if trigger >= end {
            continue;
        }
        for flag in &mut active[trigger..end] {
            *flag = true;
        }
    }

    let mut output = Vec::with_capacity(stereo.len());
    let mut prev_side = 0.0f32;
    for ((&mid_sample, &side_sample), &is_active) in mid.iter().zip(&side).zip(&active) {
        let transient = side_sample - prev_side;
        let shaped_side = if is_active {
            side_sample + transient * amount
        } else {
            side_sample
        };
        output.push(mid_sample + shaped_side);
        output.push(mid_sample - shaped_side);
        prev_side = side_sample;
    }

    output
}

fn stereo_interchannel_lag(stereo: &[f32], max_lag: usize) -> (f32, isize) {
    let left: Vec<f32> = stereo.iter().step_by(2).copied().collect();
    let right: Vec<f32> = stereo.iter().skip(1).step_by(2).copied().collect();
    best_lagged_correlation(&left, &right, max_lag)
}

fn stereo_window(stereo: &[f32], frame_start: usize, frame_len: usize) -> &[f32] {
    &stereo[frame_start * 2..(frame_start + frame_len) * 2]
}

fn master_ticks_from_output_samples(samples: usize) -> u64 {
    ((samples as u128 * u128::from(MASTER_CLOCK_NTSC) + 22_050u128) / 44_100u128) as u64
}

fn ym_pan_channel(write: &TimedYm2612Write) -> Option<usize> {
    if !(0xB4..=0xB6).contains(&write.addr) || write.port > 1 {
        return None;
    }

    let channel = usize::from(write.addr & 0x03) + usize::from(write.port) * 3;
    (channel < 6).then_some(channel)
}

fn ym_pan_state_from_value(value: u8) -> YmPanState {
    match (value & 0x80 != 0, value & 0x40 != 0) {
        (true, true) => YmPanState::Stereo,
        (true, false) => YmPanState::Left,
        (false, true) => YmPanState::Right,
        (false, false) => YmPanState::Off,
    }
}

impl YmPanStateSummary {
    fn add_ticks(&mut self, state: YmPanState, ticks: u64) {
        match state {
            YmPanState::Off => self.off_ticks += ticks,
            YmPanState::Left => self.left_ticks += ticks,
            YmPanState::Right => self.right_ticks += ticks,
            YmPanState::Stereo => self.stereo_ticks += ticks,
        }
    }
}

fn summarize_ym_pan_states(
    writes: &[TimedYm2612Write],
    start_tick: u64,
    end_tick: u64,
) -> [YmPanStateSummary; 6] {
    let mut summaries = [YmPanStateSummary::default(); 6];
    if end_tick <= start_tick {
        return summaries;
    }

    let mut states = [YmPanState::Off; 6];
    let mut cursor = start_tick;

    for write in writes {
        if write.master_tick >= end_tick {
            break;
        }

        let Some(channel) = ym_pan_channel(write) else {
            continue;
        };

        if write.master_tick < start_tick {
            states[channel] = ym_pan_state_from_value(write.value);
            continue;
        }

        if write.master_tick > cursor {
            let span = write.master_tick - cursor;
            for (summary, state) in summaries.iter_mut().zip(states) {
                summary.add_ticks(state, span);
            }
            cursor = write.master_tick;
        }

        let next_state = ym_pan_state_from_value(write.value);
        if states[channel] != next_state {
            summaries[channel].change_count += 1;
            states[channel] = next_state;
        }
    }

    if end_tick > cursor {
        let span = end_tick - cursor;
        for (summary, state) in summaries.iter_mut().zip(states) {
            summary.add_ticks(state, span);
        }
    }

    summaries
}

fn last_ym_pan_changes_before(
    writes: &[TimedYm2612Write],
    cutoff_tick: u64,
) -> [Option<(u64, YmPanState)>; 6] {
    let mut states = [YmPanState::Off; 6];
    let mut last_change = [None; 6];

    for write in writes {
        if write.master_tick >= cutoff_tick {
            break;
        }

        let Some(channel) = ym_pan_channel(write) else {
            continue;
        };

        let next_state = ym_pan_state_from_value(write.value);
        if states[channel] != next_state {
            states[channel] = next_state;
            last_change[channel] = Some((write.master_tick, next_state));
        }
    }

    last_change
}

fn force_centered_pan_writes_for_channels(
    writes: &[TimedYm2612Write],
    channel_mask: u8,
) -> Vec<TimedYm2612Write> {
    force_centered_pan_writes_for_channels_before(writes, u64::MAX, channel_mask)
}

fn force_centered_pan_writes_for_channels_before(
    writes: &[TimedYm2612Write],
    cutoff_tick: u64,
    channel_mask: u8,
) -> Vec<TimedYm2612Write> {
    writes
        .iter()
        .map(|&write| TimedYm2612Write {
            value: if write.master_tick < cutoff_tick
                && ym_pan_channel(&write)
                    .is_some_and(|channel| channel_mask & (1u8 << channel) != 0)
            {
                write.value | 0xC0
            } else {
                write.value
            },
            ..write
        })
        .collect()
}

fn force_centered_pan_writes_for_channels_between(
    writes: &[TimedYm2612Write],
    start_tick: u64,
    end_tick: u64,
    channel_mask: u8,
) -> Vec<TimedYm2612Write> {
    writes
        .iter()
        .map(|&write| TimedYm2612Write {
            value: if write.master_tick >= start_tick
                && write.master_tick < end_tick
                && ym_pan_channel(&write)
                    .is_some_and(|channel| channel_mask & (1u8 << channel) != 0)
            {
                write.value | 0xC0
            } else {
                write.value
            },
            ..write
        })
        .collect()
}

fn delay_pan_writes_for_channels(
    writes: &[TimedYm2612Write],
    delay_ticks: u64,
    channel_mask: u8,
) -> Vec<TimedYm2612Write> {
    let mut delayed: Vec<_> = writes
        .iter()
        .map(|&write| TimedYm2612Write {
            master_tick: if ym_pan_channel(&write)
                .is_some_and(|channel| channel_mask & (1u8 << channel) != 0)
            {
                write.master_tick.saturating_add(delay_ticks)
            } else {
                write.master_tick
            },
            ..write
        })
        .collect();
    delayed.sort_by_key(|write| write.master_tick);
    delayed
}

fn delay_key_writes_for_channels(
    writes: &[TimedYm2612Write],
    delay_ticks: u64,
    channel_mask: u8,
) -> Vec<TimedYm2612Write> {
    let mut delayed: Vec<_> = writes
        .iter()
        .map(|&write| TimedYm2612Write {
            master_tick: if ym_key_channel(&write)
                .is_some_and(|channel| channel_mask & (1u8 << channel) != 0)
            {
                write.master_tick.saturating_add(delay_ticks)
            } else {
                write.master_tick
            },
            ..write
        })
        .collect();
    delayed.sort_by_key(|write| write.master_tick);
    delayed
}

fn delay_pan_state_changes_for_channels(
    writes: &[TimedYm2612Write],
    delay_ticks: u64,
    channel_mask: u8,
) -> Vec<TimedYm2612Write> {
    let mut states = [YmPanState::Off; 6];
    let mut delayed = Vec::with_capacity(writes.len());

    for &write in writes {
        let mut delayed_write = write;
        if let Some(channel) = ym_pan_channel(&write) {
            let next_state = ym_pan_state_from_value(write.value);
            if channel_mask & (1u8 << channel) != 0 && states[channel] != next_state {
                delayed_write.master_tick = delayed_write.master_tick.saturating_add(delay_ticks);
            }
            states[channel] = next_state;
        }
        delayed.push(delayed_write);
    }

    delayed.sort_by_key(|write| write.master_tick);
    delayed
}

fn ym_frequency_channel(write: &TimedYm2612Write) -> Option<usize> {
    if write.port > 1 {
        return None;
    }
    let is_low = (0xA0..=0xA2).contains(&write.addr);
    let is_high = (0xA4..=0xA6).contains(&write.addr);
    if !(is_low || is_high) {
        return None;
    }

    let channel = usize::from(write.addr & 0x03) + usize::from(write.port) * 3;
    (channel < 6).then_some(channel)
}

fn ym_key_channel(write: &TimedYm2612Write) -> Option<usize> {
    if write.port != 0 || write.addr != 0x28 {
        return None;
    }

    match write.value & 0x07 {
        0..=2 => Some((write.value & 0x07) as usize),
        4..=6 => Some(((write.value & 0x07) - 4 + 3) as usize),
        _ => None,
    }
}

fn ym_key_on_channel(write: &TimedYm2612Write) -> Option<usize> {
    let channel = ym_key_channel(write)?;
    (write.value & 0xF0 != 0).then_some(channel)
}

fn key_on_trigger_samples(
    writes: &[TimedYm2612Write],
    capture_start_sample: u64,
    channel_mask: u8,
) -> Vec<usize> {
    let mut triggers = Vec::new();
    for write in writes {
        let Some(channel) = ym_key_on_channel(write) else {
            continue;
        };
        if channel_mask & (1u8 << channel) == 0 {
            continue;
        }

        let sample = samples_from_tick(write.master_tick);
        if sample < capture_start_sample {
            continue;
        }
        triggers.push((sample - capture_start_sample) as usize);
    }
    triggers
}

fn tracked_event_trigger_samples(
    events: &[Ym2612TrackedEvent],
    capture_start_sample: u64,
    kind: Ym2612TrackedEventKind,
    channel_mask: u8,
) -> Vec<usize> {
    let mut triggers = Vec::new();
    for event in events {
        if event.kind != kind || channel_mask & (1u8 << event.channel) == 0 {
            continue;
        }

        let sample = u64::from(event.sample);
        if sample < capture_start_sample {
            continue;
        }
        triggers.push((sample - capture_start_sample) as usize);
    }
    triggers
}

fn suppress_ym_frequency_writes_for_channels_between(
    writes: &[TimedYm2612Write],
    start_tick: u64,
    end_tick: u64,
    channel_mask: u8,
) -> Vec<TimedYm2612Write> {
    writes
        .iter()
        .copied()
        .filter(|write| {
            let in_window = write.master_tick >= start_tick && write.master_tick < end_tick;
            let selected = ym_frequency_channel(write)
                .is_some_and(|channel| channel_mask & (1u8 << channel) != 0);
            !(in_window && selected)
        })
        .collect()
}

fn suppress_ym_key_writes_for_channels_between(
    writes: &[TimedYm2612Write],
    start_tick: u64,
    end_tick: u64,
    channel_mask: u8,
) -> Vec<TimedYm2612Write> {
    writes
        .iter()
        .copied()
        .filter(|write| {
            let in_window = write.master_tick >= start_tick && write.master_tick < end_tick;
            let selected =
                ym_key_channel(write).is_some_and(|channel| channel_mask & (1u8 << channel) != 0);
            !(in_window && selected)
        })
        .collect()
}

fn mute_ym_pan_writes_outside_channels(
    writes: &[TimedYm2612Write],
    keep_mask: u8,
) -> Vec<TimedYm2612Write> {
    writes
        .iter()
        .map(|&write| TimedYm2612Write {
            value: if ym_pan_channel(&write)
                .is_some_and(|channel| keep_mask & (1u8 << channel) == 0)
            {
                write.value & 0x3F
            } else {
                write.value
            },
            ..write
        })
        .collect()
}

fn force_centered_pan_writes(writes: &[TimedYm2612Write]) -> Vec<TimedYm2612Write> {
    force_centered_pan_writes_for_channels(writes, 0x3F)
}

fn force_centered_pan_writes_before(
    writes: &[TimedYm2612Write],
    cutoff_tick: u64,
) -> Vec<TimedYm2612Write> {
    force_centered_pan_writes_for_channels_before(writes, cutoff_tick, 0x3F)
}

fn summarize_section_emu_positions(
    refs: &[FixedSideConsensusSectionRef],
    len: usize,
) -> Option<SectionEmuPositionSummary> {
    let min_start = refs.iter().map(|reference| reference.emu_start).min()?;
    let max_start = refs.iter().map(|reference| reference.emu_start).max()?;
    let mean_start = refs
        .iter()
        .map(|reference| reference.emu_start)
        .sum::<usize>()
        / refs.len();
    Some(SectionEmuPositionSummary {
        min_start,
        max_start,
        mean_start,
        max_end: max_start.saturating_add(len),
    })
}

fn section_tick_range_from_refs(
    trace: &GhzTimedTrace,
    refs: &[FixedSideConsensusSectionRef],
    len: usize,
) -> Option<(SectionEmuPositionSummary, u64, u64)> {
    let positions = summarize_section_emu_positions(refs, len)?;
    let start_tick =
        trace.capture_start_tick + master_ticks_from_output_samples(positions.min_start);
    let end_tick = trace.capture_start_tick + master_ticks_from_output_samples(positions.max_end);
    Some((positions, start_tick, end_tick))
}

fn section_sample_range_from_refs(
    trace: &GhzTimedTrace,
    refs: &[FixedSideConsensusSectionRef],
    len: usize,
) -> Option<(SectionEmuPositionSummary, u64, u64)> {
    let positions = summarize_section_emu_positions(refs, len)?;
    let start_sample = trace.capture_start_sample + positions.min_start as u64;
    let end_sample = trace.capture_start_sample + positions.max_end as u64;
    Some((positions, start_sample, end_sample))
}

fn summarize_ym2612_tracked_events_in_sample_range(
    events: &[Ym2612TrackedEvent],
    start_sample: u64,
    end_sample: u64,
) -> ([[u32; 3]; 6], u32) {
    let mut counts = [[0u32; 3]; 6];
    let mut total = 0u32;
    for event in events {
        let sample = u64::from(event.sample);
        if sample < start_sample || sample >= end_sample {
            continue;
        }
        let channel = usize::from(event.channel);
        if channel >= 6 {
            continue;
        }
        counts[channel][ym2612_event_kind_index(event.kind)] += 1;
        total += 1;
    }

    (counts, total)
}

fn ratio_from_ticks(ticks: u64, total_ticks: u64) -> f32 {
    if total_ticks == 0 {
        0.0
    } else {
        ticks as f32 / total_ticks as f32
    }
}

fn ms_from_master_ticks(master_ticks: u64) -> f32 {
    master_ticks as f32 * 1000.0 / MASTER_CLOCK_NTSC as f32
}

fn sum_stereo_sources(sources: &[&[f32]]) -> Vec<f32> {
    let Some(len) = sources.first().map(|source| source.len()) else {
        return Vec::new();
    };
    let mut summed = vec![0.0f32; len];
    for source in sources {
        for (dst, &sample) in summed.iter_mut().zip(source.iter()) {
            *dst += sample;
        }
    }
    summed
}

fn scale_and_clamp_stereo(stereo: &[f32], gain: f32) -> Vec<f32> {
    stereo
        .iter()
        .map(|&sample| (sample * gain).clamp(-1.0, 1.0))
        .collect()
}

fn run_frames(core: &mut GenesisCore, frames: u32) {
    for _ in 0..frames {
        core.execute(Command::StepFrame);
    }
}

fn press_start(core: &mut GenesisCore) {
    core.execute(Command::PressButton {
        port: 0,
        button: genesoxide_core::Button::Start,
    });
    core.execute(Command::StepFrame);
    core.execute(Command::ReleaseButton {
        port: 0,
        button: genesoxide_core::Button::Start,
    });
}

/// Drive Sonic from boot through the title screen and into Green Hill Zone.
///
/// The previous "skip N frames" approach never pressed Start, so the so-called
/// GHZ capture was actually comparing title-screen audio against a GHZ
/// reference. This scripted path matches the existing gameplay boot test.
fn advance_to_green_hill(core: &mut GenesisCore) {
    // Let the SEGA logo and title screen appear.
    run_frames(core, 200);
    // Start the game.
    press_start(core);
    // Allow the level transition and title card to clear enough for GHZ BGM.
    run_frames(core, 120);
    core.clear_audio_buffer();
}

/// Advance past the GHZ transition and stop near the first clearly active
/// music frame, rather than assuming a fixed frame offset is the right anchor.
fn advance_to_green_hill_music(core: &mut GenesisCore) -> u32 {
    const MAX_SEARCH_FRAMES: u32 = 480;
    const ACTIVE_RMS: f32 = 0.02;

    advance_to_green_hill(core);

    for extra_frames in 1..=MAX_SEARCH_FRAMES {
        let before = core.ym2612_write_count();
        core.execute(Command::StepFrame);
        let write_delta = core.ym2612_write_count().saturating_sub(before);
        let frame_rms = rms(core.audio_samples());

        if write_delta > 0 && frame_rms > ACTIVE_RMS {
            core.clear_audio_buffer();
            return extra_frames;
        }

        core.clear_audio_buffer();
    }

    panic!("failed to find active GHZ music window within {MAX_SEARCH_FRAMES} frames");
}

/// Compute cross-correlation coefficient between two same-length slices.
fn cross_correlation(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }

    let mean_a: f64 = a[..n].iter().map(|&x| x as f64).sum::<f64>() / n as f64;
    let mean_b: f64 = b[..n].iter().map(|&x| x as f64).sum::<f64>() / n as f64;

    let mut cov = 0.0f64;
    let mut var_a = 0.0f64;
    let mut var_b = 0.0f64;

    for i in 0..n {
        let da = a[i] as f64 - mean_a;
        let db = b[i] as f64 - mean_b;
        cov += da * db;
        var_a += da * da;
        var_b += db * db;
    }

    if var_a < 1e-12 || var_b < 1e-12 {
        return 0.0;
    }

    (cov / (var_a * var_b).sqrt()) as f32
}

/// Downsample to an RMS energy envelope so comparisons are robust to phase and
/// small timbre differences.
fn rms_envelope(samples: &[f32], window: usize) -> Vec<f32> {
    if window == 0 {
        return Vec::new();
    }

    samples
        .chunks(window)
        .filter(|chunk| !chunk.is_empty())
        .map(rms)
        .collect()
}

/// Find the best normalized correlation between two sequences while allowing a
/// bounded lag. Returns `(best_corr, lag)` where positive lag means `a` starts
/// later than `b`.
fn best_lagged_correlation(a: &[f32], b: &[f32], max_lag: usize) -> (f32, isize) {
    let mut best = (-1.0f32, 0isize);

    for lag in -(max_lag as isize)..=(max_lag as isize) {
        let (a_start, b_start) = if lag >= 0 {
            (lag as usize, 0usize)
        } else {
            (0usize, (-lag) as usize)
        };

        let overlap = a
            .len()
            .saturating_sub(a_start)
            .min(b.len().saturating_sub(b_start));
        if overlap < 64 {
            continue;
        }

        let corr = cross_correlation(
            &a[a_start..a_start + overlap],
            &b[b_start..b_start + overlap],
        );
        if corr > best.0 {
            best = (corr, lag);
        }
    }

    best
}

#[derive(Debug, Clone, Copy)]
struct WindowedMatch {
    corr: f32,
    a_start: usize,
    b_start: usize,
    len: usize,
}

#[derive(Debug, Clone, Copy)]
struct ReferenceConsistencyWindow {
    score: f32,
    prior_start: usize,
    current_start: usize,
    len: usize,
    left_spectral: f32,
    mid_spectral: f32,
    side_spectral: f32,
    rms_ratio: f32,
}

#[derive(Debug, Clone, Copy)]
struct TrustedReferenceMetrics {
    env_corr: f32,
    local_env_corr: f32,
    trusted_score: f32,
    trusted_self_left: f32,
    trusted_self_mid: f32,
    trusted_self_side: f32,
    trusted_self_rms: f32,
    trusted_emu_raw: f32,
    trusted_emu_spectral: f32,
    trusted_emu_rms: f32,
    trusted_emu_mid: f32,
    trusted_emu_side: f32,
}

#[derive(Debug, Clone, Copy)]
struct TrustedWindowSelection {
    env_corr: f32,
    local_env_corr: f32,
    trusted_score: f32,
    trusted_self_left: f32,
    trusted_self_mid: f32,
    trusted_self_side: f32,
    trusted_self_rms: f32,
    trusted_prior_start: usize,
    trusted_ref_start: usize,
    trusted_emu_start: usize,
    trusted_len: usize,
}

#[derive(Debug, Clone, Copy)]
struct FixedTrustedWindowMetrics {
    raw: f32,
    spectral: f32,
    rms_ratio: f32,
    mid: f32,
    side: f32,
}

#[derive(Debug, Clone)]
struct ConsensusCandidateScore {
    name: String,
    average_score: f32,
    disagreement_penalty: f32,
    final_score: f32,
}

#[derive(Debug, Clone, Copy)]
struct SectionedReferenceScore {
    normalized_score: f32,
    section_reliability: f32,
    worst_section_score: f32,
}

#[derive(Debug, Clone, Copy)]
struct MonoFirstSectionedReferenceScore {
    mono_score: f32,
    side_score: f32,
    combined_score: f32,
    section_reliability: f32,
    worst_mono_section_score: f32,
    worst_side_section_score: f32,
}

#[derive(Debug, Clone)]
struct MonoConsensusWindow {
    spectrum: Vec<f32>,
    rms: f32,
    weight: f32,
}

#[derive(Debug, Clone)]
struct SideConsensusWindow {
    spectrum: Vec<f32>,
    ratio: f32,
    weight: f32,
}

#[derive(Debug, Clone)]
struct SideDynamicsWindow {
    envelope: Vec<f32>,
    ratio: f32,
    weight: f32,
}

#[derive(Debug, Clone)]
struct MonoConsensusTarget {
    spectrum: Vec<f32>,
    rms: f32,
    self_spectral: f32,
    self_rms_fit: f32,
}

#[derive(Debug, Clone)]
struct SideConsensusTarget {
    spectrum: Vec<f32>,
    ratio: f32,
    self_spectral: f32,
    self_ratio_fit: f32,
}

#[derive(Debug, Clone)]
struct SideDynamicsTarget {
    envelope: Vec<f32>,
    ratio: f32,
    self_envelope: f32,
    self_ratio_fit: f32,
}

#[derive(Debug, Clone)]
struct FixedMonoConsensusReference {
    name: String,
    ref_index: usize,
    selection: TrustedWindowSelection,
    weight: f32,
    side_weight: f32,
}

#[derive(Debug, Clone)]
struct FixedSideConsensusSectionRef {
    ref_index: usize,
    ref_start: usize,
    emu_start: usize,
    weight: f32,
}

#[derive(Debug, Clone)]
struct FixedSideConsensusSectionTarget {
    start: usize,
    len: usize,
    target: SideConsensusTarget,
    refs: Vec<FixedSideConsensusSectionRef>,
    weight: f32,
    transient_reliability: f32,
}

#[derive(Debug, Clone)]
struct FixedSideDynamicsSectionTarget {
    start: usize,
    len: usize,
    target: SideDynamicsTarget,
    refs: Vec<FixedSideConsensusSectionRef>,
    weight: f32,
}

#[derive(Debug, Clone)]
struct ReferenceAuthoritySummary {
    name: String,
    mono_manual_weight: f32,
    side_manual_weight: f32,
    mono_effective_weight: f32,
    side_consensus_effective_weight: f32,
    side_dynamics_effective_weight: f32,
}

#[derive(Debug, Clone, Copy)]
struct OracleScoreSnapshot {
    mono_final: f32,
    hybrid_final: f32,
    side_consensus_final: f32,
    side_dynamics_final: f32,
}

#[derive(Debug, Clone, Copy)]
struct SectionedSideConsensusScore {
    average_score: f32,
    disagreement_penalty: f32,
    final_score: f32,
    worst_section_score: f32,
    dominant_section_impact: f32,
}

#[derive(Debug, Clone, Copy)]
struct SectionedSideDynamicsScore {
    average_score: f32,
    disagreement_penalty: f32,
    final_score: f32,
    worst_section_score: f32,
    dominant_section_impact: f32,
}

#[derive(Debug, Clone)]
struct AnalyzedSideConsensusSection {
    average_score: f32,
    disagreement_penalty: f32,
    final_score: f32,
    candidate: SideConsensusWindow,
}

#[derive(Debug, Clone)]
struct AnalyzedSideDynamicsSection {
    average_score: f32,
    disagreement_penalty: f32,
    final_score: f32,
    candidate: SideDynamicsWindow,
}

#[derive(Debug, Clone, Copy)]
struct TransientAlignmentMetrics {
    envelope_corr: f32,
    lag_bins: isize,
    adjusted_envelope_corr: f32,
    transient_corr: f32,
    adjusted_transient_corr: f32,
    transient_rms_ratio: f32,
}

#[derive(Debug, Clone)]
struct ReferenceSideDynamicsPairAnalysis {
    left_name: String,
    right_name: String,
    weight: f32,
    left_ratio: f32,
    right_ratio: f32,
    metrics: TransientAlignmentMetrics,
}

#[derive(Debug, Clone)]
struct ReferenceSideConsensusPairAnalysis {
    weight: f32,
    spectral_similarity: f32,
    ratio_fit: f32,
}

#[derive(Debug, Clone, Copy)]
struct ReferenceSideConsensusConsistency {
    pair_count: usize,
    average_spectral_similarity: f32,
    average_ratio_fit: f32,
    worst_spectral_similarity: f32,
}

#[derive(Debug, Clone, Copy)]
struct ReferenceSideDynamicsConsistency {
    pair_count: usize,
    average_envelope_corr: f32,
    average_adjusted_envelope_corr: f32,
    average_transient_corr: f32,
    average_adjusted_transient_corr: f32,
    average_transient_rms_fit: f32,
    average_abs_lag_bins: f32,
    worst_adjusted_transient_corr: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SectionEmuPositionSummary {
    min_start: usize,
    max_start: usize,
    mean_start: usize,
    max_end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum YmPanState {
    Off,
    Left,
    Right,
    Stereo,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct YmPanStateSummary {
    off_ticks: u64,
    left_ticks: u64,
    right_ticks: u64,
    stereo_ticks: u64,
    change_count: u32,
}

fn best_windowed_match_at_lag(
    a: &[f32],
    b: &[f32],
    lag: isize,
    window_len: usize,
) -> Option<WindowedMatch> {
    if window_len == 0 {
        return None;
    }

    let (a_base, b_base) = if lag >= 0 {
        (lag as usize, 0usize)
    } else {
        (0usize, (-lag) as usize)
    };

    let overlap = a
        .len()
        .saturating_sub(a_base)
        .min(b.len().saturating_sub(b_base));
    if overlap < window_len {
        return None;
    }

    let mut best: Option<WindowedMatch> = None;
    for offset in 0..=overlap - window_len {
        let a_start = a_base + offset;
        let b_start = b_base + offset;
        let corr = cross_correlation(
            &a[a_start..a_start + window_len],
            &b[b_start..b_start + window_len],
        );

        if best.is_none_or(|candidate| corr > candidate.corr) {
            best = Some(WindowedMatch {
                corr,
                a_start,
                b_start,
                len: window_len,
            });
        }
    }

    best
}

fn reference_self_window_at_offset(
    reference_stereo: &[f32],
    prior_start: usize,
    current_start: usize,
    window_len: usize,
    frame_len: usize,
    hop_len: usize,
    bins: &[usize],
) -> Option<ReferenceConsistencyWindow> {
    let total_frames = reference_stereo.len() / 2;
    if window_len == 0
        || prior_start
            .checked_add(window_len)
            .is_none_or(|end| end > total_frames)
        || current_start
            .checked_add(window_len)
            .is_none_or(|end| end > total_frames)
    {
        return None;
    }

    let prior_stereo = stereo_window(reference_stereo, prior_start, window_len);
    let current_stereo = stereo_window(reference_stereo, current_start, window_len);
    let (prior_left, _) = stereo_left_right(prior_stereo);
    let (current_left, _) = stereo_left_right(current_stereo);
    let (prior_mid, prior_side) = stereo_mid_side(prior_stereo);
    let (current_mid, current_side) = stereo_mid_side(current_stereo);
    let left_spectral = spectral_similarity(&prior_left, &current_left, frame_len, hop_len, bins);
    let mid_spectral = spectral_similarity(&prior_mid, &current_mid, frame_len, hop_len, bins);
    let side_spectral = spectral_similarity(&prior_side, &current_side, frame_len, hop_len, bins);
    let rms_ratio = rms(&current_left) / rms(&prior_left).max(1e-9);
    let rms_fit = 1.0 / (1.0 + (rms_ratio - 1.0).abs());
    let score = (left_spectral + mid_spectral + rms_fit + side_spectral * 0.25) / 3.25;

    Some(ReferenceConsistencyWindow {
        score,
        prior_start,
        current_start,
        len: window_len,
        left_spectral,
        mid_spectral,
        side_spectral,
        rms_ratio,
    })
}

fn best_reference_self_window_at_offset(
    reference_stereo: &[f32],
    offset_frames: usize,
    window_len: usize,
    frame_len: usize,
    hop_len: usize,
    bins: &[usize],
) -> Option<ReferenceConsistencyWindow> {
    let total_frames = reference_stereo.len() / 2;
    if offset_frames == 0 || window_len == 0 || total_frames < offset_frames + window_len {
        return None;
    }

    let mut best = None;
    let search_step = hop_len.max(1);
    for current_start in (offset_frames..=total_frames - window_len).step_by(search_step) {
        let prior_start = current_start - offset_frames;
        let Some(candidate) = reference_self_window_at_offset(
            reference_stereo,
            prior_start,
            current_start,
            window_len,
            frame_len,
            hop_len,
            bins,
        ) else {
            continue;
        };
        if best.is_none_or(|current: ReferenceConsistencyWindow| candidate.score > current.score) {
            best = Some(candidate);
        }
    }
    let last_start = total_frames - window_len;
    if (last_start - offset_frames) % search_step != 0 {
        let prior_start = last_start - offset_frames;
        if let Some(candidate) = reference_self_window_at_offset(
            reference_stereo,
            prior_start,
            last_start,
            window_len,
            frame_len,
            hop_len,
            bins,
        ) {
            if best
                .is_none_or(|current: ReferenceConsistencyWindow| candidate.score > current.score)
            {
                best = Some(candidate);
            }
        }
    }

    best
}

fn discover_ghz_reference_paths() -> Vec<PathBuf> {
    let ref_dir = Path::new(REFERENCE_DIR);
    let mut refs: Vec<_> = std::fs::read_dir(ref_dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            let ext = path.extension().and_then(|e| e.to_str());
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            matches!(ext, Some("flac" | "wav"))
                && stem.starts_with("sonic_ghz")
                && !stem.contains("_emu")
        })
        .collect();
    refs.sort();
    refs
}

fn resolve_ghz_references(
    discovered: &[PathBuf],
    manifest: Option<&GhzReferenceManifest>,
) -> Vec<ResolvedGhzReference> {
    let mut available = std::collections::BTreeMap::<String, PathBuf>::new();
    for path in discovered {
        let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        available
            .entry(filename.to_owned())
            .or_insert_with(|| path.clone());
    }

    let mut resolved = Vec::new();
    let mut disabled = std::collections::HashSet::<String>::new();

    if let Some(manifest) = manifest {
        for entry in &manifest.references {
            if !entry.enabled {
                disabled.insert(entry.filename.clone());
                continue;
            }

            let Some(path) = available.remove(&entry.filename) else {
                continue;
            };
            resolved.push(ResolvedGhzReference {
                path,
                mono_weight: entry.mono_weight,
                side_weight: entry.side_weight,
            });
        }
    }

    for (filename, path) in available {
        if disabled.contains(&filename) {
            continue;
        }
        resolved.push(ResolvedGhzReference {
            path,
            mono_weight: 1.0,
            side_weight: 1.0,
        });
    }

    resolved
}

fn ghz_reference_files() -> Vec<ResolvedGhzReference> {
    resolve_ghz_references(
        &discover_ghz_reference_paths(),
        load_ghz_reference_manifest(),
    )
}

fn ghz_reference_weight(path: &Path) -> GhzReferenceWeight {
    let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
        return GhzReferenceWeight {
            mono: 1.0,
            side: 1.0,
        };
    };

    let Some(manifest) = load_ghz_reference_manifest() else {
        return GhzReferenceWeight {
            mono: 1.0,
            side: 1.0,
        };
    };

    manifest
        .references
        .iter()
        .find(|entry| entry.enabled && entry.filename == filename)
        .map(|entry| GhzReferenceWeight {
            mono: entry.mono_weight,
            side: entry.side_weight,
        })
        .unwrap_or(GhzReferenceWeight {
            mono: 1.0,
            side: 1.0,
        })
}

fn ghz_reference_weight_with_side_overrides(
    path: &Path,
    side_overrides: &std::collections::BTreeMap<String, f32>,
) -> GhzReferenceWeight {
    let mut weight = ghz_reference_weight(path);
    let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
        return weight;
    };

    if let Some(&side_weight) = side_overrides.get(filename) {
        weight.side = sanitize_ghz_reference_weight(side_weight);
    }

    weight
}

fn weighted_section_reference_average(
    refs_by_section: impl IntoIterator<Item = (f32, Vec<FixedSideConsensusSectionRef>)>,
    ref_index: usize,
) -> f32 {
    let mut weighted_sum = 0.0f32;
    let mut total_weight = 0.0f32;
    for (section_weight, refs) in refs_by_section {
        let Some(reference) = refs
            .into_iter()
            .find(|reference| reference.ref_index == ref_index)
        else {
            continue;
        };
        weighted_sum += reference.weight * section_weight;
        total_weight += section_weight;
    }

    if total_weight <= 1e-6 {
        0.0
    } else {
        weighted_sum / total_weight
    }
}

fn summarize_reference_authority(
    loaded_refs: &[(PathBuf, Vec<f32>)],
    mono_refs: &[FixedMonoConsensusReference],
    side_targets: &[FixedSideConsensusSectionTarget],
    dynamics_targets: &[FixedSideDynamicsSectionTarget],
) -> Vec<ReferenceAuthoritySummary> {
    loaded_refs
        .iter()
        .enumerate()
        .map(|(ref_index, (path, _samples))| {
            let manual_weight = ghz_reference_weight(path);
            let mono_effective_weight = mono_refs
                .iter()
                .find(|reference| reference.ref_index == ref_index)
                .map_or(0.0, |reference| reference.weight);
            let side_consensus_effective_weight = weighted_section_reference_average(
                side_targets
                    .iter()
                    .map(|section| (section.weight, section.refs.clone())),
                ref_index,
            );
            let side_dynamics_effective_weight = weighted_section_reference_average(
                dynamics_targets
                    .iter()
                    .map(|section| (section.weight, section.refs.clone())),
                ref_index,
            );

            ReferenceAuthoritySummary {
                name: path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("unknown")
                    .to_owned(),
                mono_manual_weight: manual_weight.mono,
                side_manual_weight: manual_weight.side,
                mono_effective_weight,
                side_consensus_effective_weight,
                side_dynamics_effective_weight,
            }
        })
        .collect()
}

fn ghz_reference_paths() -> Vec<PathBuf> {
    ghz_reference_files()
        .into_iter()
        .map(|reference| reference.path)
        .collect()
}

fn select_trusted_reference_window(
    emu_samples: &[f32],
    ref_samples: &[f32],
) -> Option<TrustedWindowSelection> {
    let emu_left: Vec<f32> = emu_samples.iter().step_by(2).copied().collect();
    let ref_left: Vec<f32> = ref_samples.iter().step_by(2).copied().collect();
    let env_window = 2048usize;
    let emu_env = rms_envelope(&emu_left, env_window);
    let ref_env = rms_envelope(&ref_left, env_window);
    let max_lag_windows = emu_env.len().min(ref_env.len()).saturating_sub(64);
    let (env_corr, env_lag) = best_lagged_correlation(&emu_env, &ref_env, max_lag_windows);
    let local_env_window = (SAMPLE_RATE as usize * 8 / env_window).max(64);
    let local_match = best_windowed_match_at_lag(&emu_env, &ref_env, env_lag, local_env_window)?;
    let loop_offset_bins = local_match.b_start.abs_diff(local_match.a_start);
    let trusted_env = best_windowed_match_at_lag(
        &ref_env,
        &ref_env,
        loop_offset_bins as isize,
        local_env_window,
    )?;
    let spectral_bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 12_000.0, 24);
    let trusted_prior_start = trusted_env.b_start * env_window;
    let trusted_ref_start = trusted_env.a_start * env_window;
    let trusted_len = (trusted_env.len * env_window)
        .min(ref_left.len().saturating_sub(trusted_prior_start))
        .min(ref_left.len().saturating_sub(trusted_ref_start));
    let lag_samples = env_lag * env_window as isize;
    let trusted_emu_start = trusted_ref_start as isize + lag_samples;
    if trusted_emu_start < 0 {
        return None;
    }
    let trusted_emu_start = trusted_emu_start as usize;
    if trusted_emu_start + trusted_len > emu_left.len() {
        return None;
    }

    let trusted_self = reference_self_window_at_offset(
        ref_samples,
        trusted_prior_start,
        trusted_ref_start,
        trusted_len,
        2048,
        1024,
        &spectral_bins,
    )?;

    Some(TrustedWindowSelection {
        env_corr,
        local_env_corr: local_match.corr,
        trusted_score: trusted_self.score,
        trusted_self_left: trusted_self.left_spectral,
        trusted_self_mid: trusted_self.mid_spectral,
        trusted_self_side: trusted_self.side_spectral,
        trusted_self_rms: trusted_self.rms_ratio,
        trusted_prior_start,
        trusted_ref_start,
        trusted_emu_start,
        trusted_len,
    })
}

fn score_fixed_trusted_window(
    emu_samples: &[f32],
    ref_samples: &[f32],
    selection: TrustedWindowSelection,
) -> Option<FixedTrustedWindowMetrics> {
    if selection.trusted_len == 0 {
        return None;
    }
    let emu_left: Vec<f32> = emu_samples.iter().step_by(2).copied().collect();
    let ref_left: Vec<f32> = ref_samples.iter().step_by(2).copied().collect();
    if selection.trusted_emu_start + selection.trusted_len > emu_left.len()
        || selection.trusted_ref_start + selection.trusted_len > ref_left.len()
    {
        return None;
    }

    let trusted_ref_stereo = stereo_window(
        ref_samples,
        selection.trusted_ref_start,
        selection.trusted_len,
    );
    let trusted_emu_stereo = stereo_window(
        emu_samples,
        selection.trusted_emu_start,
        selection.trusted_len,
    );
    let (ref_left_window, _) = stereo_left_right(trusted_ref_stereo);
    let (ref_mid, ref_side) = stereo_mid_side(trusted_ref_stereo);
    let (emu_mid, emu_side) = stereo_mid_side(trusted_emu_stereo);
    let (emu_left_window, _) = stereo_left_right(trusted_emu_stereo);
    let spectral_bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 12_000.0, 24);

    Some(FixedTrustedWindowMetrics {
        raw: cross_correlation(&emu_left_window, &ref_left_window),
        spectral: spectral_similarity(
            &emu_left_window,
            &ref_left_window,
            2048,
            1024,
            &spectral_bins,
        ),
        rms_ratio: rms(&emu_left_window) / rms(&ref_left_window).max(1e-9),
        mid: spectral_similarity(&emu_mid, &ref_mid, 2048, 1024, &spectral_bins),
        side: spectral_similarity(&emu_side, &ref_side, 2048, 1024, &spectral_bins),
    })
}

fn fixed_trusted_window_score(
    metrics: FixedTrustedWindowMetrics,
    selection: TrustedWindowSelection,
) -> f32 {
    let spectral_score = normalized_score(metrics.spectral, selection.trusted_self_left);
    let mid_score = normalized_score(metrics.mid, selection.trusted_self_mid);
    let side_score = normalized_score(metrics.side, selection.trusted_self_side);
    let rms_score = normalized_score(
        rms_fit_score(metrics.rms_ratio),
        rms_fit_score(selection.trusted_self_rms),
    );

    (spectral_score + mid_score + side_score + rms_score) / 4.0
}

fn analyze_trusted_reference_window(
    emu_samples: &[f32],
    ref_samples: &[f32],
) -> Option<TrustedReferenceMetrics> {
    let selection = select_trusted_reference_window(emu_samples, ref_samples)?;
    let fixed = score_fixed_trusted_window(emu_samples, ref_samples, selection)?;

    Some(TrustedReferenceMetrics {
        env_corr: selection.env_corr,
        local_env_corr: selection.local_env_corr,
        trusted_score: selection.trusted_score,
        trusted_self_left: selection.trusted_self_left,
        trusted_self_mid: selection.trusted_self_mid,
        trusted_self_side: selection.trusted_self_side,
        trusted_self_rms: selection.trusted_self_rms,
        trusted_emu_raw: fixed.raw,
        trusted_emu_spectral: fixed.spectral,
        trusted_emu_rms: fixed.rms_ratio,
        trusted_emu_mid: fixed.mid,
        trusted_emu_side: fixed.side,
    })
}

fn rms_fit_score(rms_ratio: f32) -> f32 {
    1.0 / (1.0 + (rms_ratio - 1.0).abs())
}

fn trusted_reference_score(metrics: TrustedReferenceMetrics) -> f32 {
    let rms_fit = rms_fit_score(metrics.trusted_emu_rms);
    (metrics.local_env_corr
        + metrics.trusted_emu_spectral
        + metrics.trusted_emu_mid
        + metrics.trusted_emu_side
        + rms_fit)
        / 5.0
}

fn normalized_score(value: f32, self_ceiling: f32) -> f32 {
    if self_ceiling <= 1e-6 {
        return 0.0;
    }
    (value / self_ceiling).clamp(0.0, 1.0)
}

fn normalized_rms_fit_score(metrics: TrustedReferenceMetrics) -> f32 {
    normalized_score(
        rms_fit_score(metrics.trusted_emu_rms),
        rms_fit_score(metrics.trusted_self_rms),
    )
}

fn normalized_reference_score(metrics: TrustedReferenceMetrics) -> f32 {
    let env_score = normalized_score(metrics.local_env_corr, metrics.trusted_score);
    let spectral_score = normalized_score(metrics.trusted_emu_spectral, metrics.trusted_self_left);
    let mid_score = normalized_score(metrics.trusted_emu_mid, metrics.trusted_self_mid);
    let side_score = normalized_score(metrics.trusted_emu_side, metrics.trusted_self_side);
    let rms_score = normalized_rms_fit_score(metrics);

    (env_score + spectral_score + mid_score + side_score + rms_score) / 5.0
}

fn self_consistency_weight(
    trusted_score: f32,
    trusted_self_left: f32,
    trusted_self_mid: f32,
    trusted_self_side: f32,
    trusted_self_rms: f32,
) -> f32 {
    let self_rms_fit = rms_fit_score(trusted_self_rms);
    let stability = (trusted_score * 0.30
        + trusted_self_left * 0.15
        + trusted_self_mid * 0.15
        + trusted_self_side * 0.30
        + self_rms_fit * 0.10)
        .clamp(0.0, 1.0);

    0.20 + 0.80 * stability * stability
}

fn reference_reliability_weight(metrics: TrustedReferenceMetrics) -> f32 {
    self_consistency_weight(
        metrics.trusted_score,
        metrics.trusted_self_left,
        metrics.trusted_self_mid,
        metrics.trusted_self_side,
        metrics.trusted_self_rms,
    )
}

fn trusted_window_reliability_weight(selection: TrustedWindowSelection) -> f32 {
    self_consistency_weight(
        selection.trusted_score,
        selection.trusted_self_left,
        selection.trusted_self_mid,
        selection.trusted_self_side,
        selection.trusted_self_rms,
    )
}

fn weighted_average(values: &[f32], weights: &[f32]) -> f32 {
    let weighted_sum: f32 = values.iter().zip(weights).map(|(&v, &w)| v * w).sum();
    let weight_sum: f32 = weights.iter().sum();
    if weight_sum <= 1e-6 {
        return 0.0;
    }
    weighted_sum / weight_sum
}

fn dominant_pan_behavior_score(
    hybrid_final: f32,
    dominant_side_final: f32,
    dominant_dynamics_final: f32,
) -> f32 {
    hybrid_final * 0.99 + dominant_side_final * 0.005 + dominant_dynamics_final * 0.005
}

fn weighted_mean_absolute_deviation(values: &[f32], weights: &[f32]) -> f32 {
    if values.is_empty() || values.len() != weights.len() {
        return 0.0;
    }
    let mean = weighted_average(values, weights);
    let weighted_sum: f32 = values
        .iter()
        .zip(weights)
        .map(|(&v, &w)| (v - mean).abs() * w)
        .sum();
    let weight_sum: f32 = weights.iter().sum();
    if weight_sum <= 1e-6 {
        return 0.0;
    }
    weighted_sum / weight_sum
}

fn score_consensus_candidate(
    name: &str,
    metrics: &[TrustedReferenceMetrics],
) -> ConsensusCandidateScore {
    let weights: Vec<f32> = metrics
        .iter()
        .copied()
        .map(reference_reliability_weight)
        .collect();
    let normalized_scores: Vec<f32> = metrics
        .iter()
        .copied()
        .map(normalized_reference_score)
        .collect();
    let spectral_scores: Vec<f32> = metrics
        .iter()
        .map(|&m| normalized_score(m.trusted_emu_spectral, m.trusted_self_left))
        .collect();
    let side_scores: Vec<f32> = metrics
        .iter()
        .map(|&m| normalized_score(m.trusted_emu_side, m.trusted_self_side))
        .collect();
    let rms_scores: Vec<f32> = metrics
        .iter()
        .copied()
        .map(normalized_rms_fit_score)
        .collect();

    let average_score = weighted_average(&normalized_scores, &weights);
    let disagreement_penalty = weighted_mean_absolute_deviation(&spectral_scores, &weights) * 0.35
        + weighted_mean_absolute_deviation(&side_scores, &weights) * 0.40
        + weighted_mean_absolute_deviation(&rms_scores, &weights) * 0.25;

    ConsensusCandidateScore {
        name: name.to_owned(),
        average_score,
        disagreement_penalty,
        final_score: average_score - disagreement_penalty,
    }
}

fn score_sectioned_reference(
    emu_samples: &[f32],
    ref_samples: &[f32],
    selection: TrustedWindowSelection,
    section_len: usize,
) -> Option<SectionedReferenceScore> {
    let spectral_bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 12_000.0, 24);
    let mut section_scores = Vec::new();
    let mut section_weights = Vec::new();

    for (start, len) in partition_window_sections(selection.trusted_len, section_len) {
        if len < 2048 {
            continue;
        }

        let aligned = score_aligned_section(
            emu_samples,
            ref_samples,
            selection.trusted_emu_start + start,
            selection.trusted_ref_start + start,
            len,
        )?;
        let self_window = reference_self_window_at_offset(
            ref_samples,
            selection.trusted_prior_start + start,
            selection.trusted_ref_start + start,
            len,
            2048,
            1024,
            &spectral_bins,
        )?;
        section_scores.push(normalized_section_score(aligned, self_window));
        section_weights.push(section_self_consistency_weight(self_window));
    }

    if section_scores.is_empty() {
        return None;
    }

    let uniform = vec![1.0; section_weights.len()];
    Some(SectionedReferenceScore {
        normalized_score: weighted_average(&section_scores, &section_weights),
        section_reliability: weighted_average(&section_weights, &uniform),
        worst_section_score: section_scores.iter().copied().fold(f32::INFINITY, f32::min),
    })
}

fn score_sectioned_consensus_candidate(
    name: &str,
    metrics: &[TrustedReferenceMetrics],
    sectioned_scores: &[SectionedReferenceScore],
) -> ConsensusCandidateScore {
    if metrics.is_empty() || metrics.len() != sectioned_scores.len() {
        return ConsensusCandidateScore {
            name: name.to_owned(),
            average_score: 0.0,
            disagreement_penalty: 0.0,
            final_score: 0.0,
        };
    }

    let weights: Vec<f32> = metrics
        .iter()
        .zip(sectioned_scores)
        .map(|(&metric, &sectioned)| {
            reference_reliability_weight(metric)
                * sectioned.section_reliability
                * sectioned.section_reliability
        })
        .collect();
    let normalized_scores: Vec<f32> = sectioned_scores
        .iter()
        .map(|score| score.normalized_score)
        .collect();
    let worst_scores: Vec<f32> = sectioned_scores
        .iter()
        .map(|score| score.worst_section_score)
        .collect();

    let average_score = weighted_average(&normalized_scores, &weights);
    let disagreement_penalty = weighted_mean_absolute_deviation(&normalized_scores, &weights)
        * 0.65
        + weighted_mean_absolute_deviation(&worst_scores, &weights) * 0.35;

    ConsensusCandidateScore {
        name: name.to_owned(),
        average_score,
        disagreement_penalty,
        final_score: average_score - disagreement_penalty,
    }
}

fn score_mono_first_sectioned_reference(
    emu_samples: &[f32],
    ref_samples: &[f32],
    selection: TrustedWindowSelection,
    section_len: usize,
) -> Option<MonoFirstSectionedReferenceScore> {
    let spectral_bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 12_000.0, 24);
    let mut mono_scores = Vec::new();
    let mut side_scores = Vec::new();
    let mut combined_scores = Vec::new();
    let mut section_weights = Vec::new();

    for (start, len) in partition_window_sections(selection.trusted_len, section_len) {
        if len < 2048 {
            continue;
        }

        let aligned = score_aligned_section(
            emu_samples,
            ref_samples,
            selection.trusted_emu_start + start,
            selection.trusted_ref_start + start,
            len,
        )?;
        let self_window = reference_self_window_at_offset(
            ref_samples,
            selection.trusted_prior_start + start,
            selection.trusted_ref_start + start,
            len,
            2048,
            1024,
            &spectral_bins,
        )?;
        let mono_score = normalized_section_mono_score(aligned, self_window);
        let side_score = normalized_section_side_score(aligned, self_window);
        mono_scores.push(mono_score);
        side_scores.push(side_score);
        combined_scores.push(mono_score * 0.85 + side_score * 0.15);
        section_weights.push(mono_first_section_self_consistency_weight(self_window));
    }

    if combined_scores.is_empty() {
        return None;
    }

    let uniform = vec![1.0; section_weights.len()];
    Some(MonoFirstSectionedReferenceScore {
        mono_score: weighted_average(&mono_scores, &section_weights),
        side_score: weighted_average(&side_scores, &section_weights),
        combined_score: weighted_average(&combined_scores, &section_weights),
        section_reliability: weighted_average(&section_weights, &uniform),
        worst_mono_section_score: mono_scores.iter().copied().fold(f32::INFINITY, f32::min),
        worst_side_section_score: side_scores.iter().copied().fold(f32::INFINITY, f32::min),
    })
}

fn score_mono_first_consensus_candidate(
    name: &str,
    metrics: &[TrustedReferenceMetrics],
    mono_first_scores: &[MonoFirstSectionedReferenceScore],
) -> ConsensusCandidateScore {
    if metrics.is_empty() || metrics.len() != mono_first_scores.len() {
        return ConsensusCandidateScore {
            name: name.to_owned(),
            average_score: 0.0,
            disagreement_penalty: 0.0,
            final_score: 0.0,
        };
    }

    let weights: Vec<f32> = metrics
        .iter()
        .zip(mono_first_scores)
        .map(|(&metric, &score)| {
            reference_reliability_weight(metric)
                * score.section_reliability
                * score.section_reliability
        })
        .collect();
    let combined_scores: Vec<f32> = mono_first_scores
        .iter()
        .map(|score| score.combined_score)
        .collect();
    let mono_scores: Vec<f32> = mono_first_scores
        .iter()
        .map(|score| score.mono_score)
        .collect();
    let side_scores: Vec<f32> = mono_first_scores
        .iter()
        .map(|score| score.side_score)
        .collect();
    let worst_mono_scores: Vec<f32> = mono_first_scores
        .iter()
        .map(|score| score.worst_mono_section_score)
        .collect();
    let worst_side_scores: Vec<f32> = mono_first_scores
        .iter()
        .map(|score| score.worst_side_section_score)
        .collect();

    let average_score = weighted_average(&combined_scores, &weights);
    let disagreement_penalty = weighted_mean_absolute_deviation(&mono_scores, &weights) * 0.55
        + weighted_mean_absolute_deviation(&side_scores, &weights) * 0.10
        + weighted_mean_absolute_deviation(&worst_mono_scores, &weights) * 0.25
        + weighted_mean_absolute_deviation(&worst_side_scores, &weights) * 0.10;

    ConsensusCandidateScore {
        name: name.to_owned(),
        average_score,
        disagreement_penalty,
        final_score: average_score - disagreement_penalty,
    }
}

fn build_fixed_mono_consensus_target_with_weight_resolver<F>(
    anchor_rendered: &[f32],
    loaded_refs: &[(std::path::PathBuf, Vec<f32>)],
    mut weight_resolver: F,
) -> Option<(MonoConsensusTarget, Vec<FixedMonoConsensusReference>)>
where
    F: FnMut(&Path) -> GhzReferenceWeight,
{
    let spectral_bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 12_000.0, 24);
    let mut refs = Vec::new();
    let mut windows = Vec::new();

    for (ref_index, (path, ref_samples)) in loaded_refs.iter().enumerate() {
        let selection = select_trusted_reference_window(anchor_rendered, ref_samples)?;
        let manual_weight = weight_resolver(path);
        let weight = mono_consensus_reference_weight(selection) * manual_weight.mono;
        let side_weight = trusted_window_reliability_weight(selection) * manual_weight.side;
        let window = extract_mono_consensus_window(
            ref_samples,
            selection.trusted_ref_start,
            selection.trusted_len,
            2048,
            1024,
            &spectral_bins,
            weight,
        )?;
        refs.push(FixedMonoConsensusReference {
            name: path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_owned(),
            ref_index,
            selection,
            weight,
            side_weight,
        });
        windows.push(window);
    }

    Some((build_mono_consensus_target(&windows)?, refs))
}

fn build_fixed_mono_consensus_target(
    anchor_rendered: &[f32],
    loaded_refs: &[(std::path::PathBuf, Vec<f32>)],
) -> Option<(MonoConsensusTarget, Vec<FixedMonoConsensusReference>)> {
    build_fixed_mono_consensus_target_with_weight_resolver(anchor_rendered, loaded_refs, |path| {
        ghz_reference_weight(path)
    })
}

fn extract_fixed_mono_consensus_candidate_windows(
    rendered: &[f32],
    refs: &[FixedMonoConsensusReference],
) -> Option<Vec<(String, MonoConsensusWindow)>> {
    let spectral_bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 12_000.0, 24);
    let mut windows = Vec::new();
    for reference in refs {
        let window = extract_mono_consensus_window(
            rendered,
            reference.selection.trusted_emu_start,
            reference.selection.trusted_len,
            2048,
            1024,
            &spectral_bins,
            reference.weight,
        )?;
        windows.push((reference.name.clone(), window));
    }
    Some(windows)
}

fn score_fixed_mono_consensus_candidate(
    name: &str,
    rendered: &[f32],
    refs: &[FixedMonoConsensusReference],
    target: &MonoConsensusTarget,
) -> Option<ConsensusCandidateScore> {
    let windows = extract_fixed_mono_consensus_candidate_windows(rendered, refs)?;
    let windows_only: Vec<_> = windows.into_iter().map(|(_, window)| window).collect();
    Some(score_mono_consensus_candidate_windows(
        name,
        &windows_only,
        target,
    ))
}

fn score_fixed_hybrid_mono_consensus_candidate(
    name: &str,
    rendered: &[f32],
    refs: &[FixedMonoConsensusReference],
    loaded_refs: &[(std::path::PathBuf, Vec<f32>)],
    target: &MonoConsensusTarget,
) -> Option<ConsensusCandidateScore> {
    let mono_score = score_fixed_mono_consensus_candidate(name, rendered, refs, target)?;
    let side_targets =
        build_fixed_sectioned_side_consensus(rendered, loaded_refs, SAMPLE_RATE as usize);
    let side_reliability = side_targets
        .as_ref()
        .map(|sections| sectioned_side_transient_reliability(sections))
        .unwrap_or(1.0);
    let mut side_scores = Vec::new();
    let mut side_weights = Vec::new();
    for reference in refs {
        let ref_samples = &loaded_refs.get(reference.ref_index)?.1;
        let metrics = score_fixed_trusted_window(rendered, ref_samples, reference.selection)?;
        side_scores.push(normalized_score(
            metrics.side,
            reference.selection.trusted_self_side,
        ));
        side_weights.push(reference.side_weight);
    }
    Some(score_hybrid_mono_consensus_candidate(
        name,
        mono_score,
        &side_scores,
        &side_weights,
        side_reliability,
    ))
}

fn score_fixed_oracle_snapshot(
    rendered: &[f32],
    loaded_refs: &[(PathBuf, Vec<f32>)],
) -> Option<OracleScoreSnapshot> {
    score_fixed_oracle_snapshot_with_weight_resolver(rendered, loaded_refs, |path| {
        ghz_reference_weight(path)
    })
}

fn score_fixed_oracle_snapshot_with_weight_resolver<F>(
    rendered: &[f32],
    loaded_refs: &[(PathBuf, Vec<f32>)],
    weight_resolver: F,
) -> Option<OracleScoreSnapshot>
where
    F: FnMut(&Path) -> GhzReferenceWeight + Copy,
{
    let (mono_target, mono_refs) = build_fixed_mono_consensus_target_with_weight_resolver(
        rendered,
        loaded_refs,
        weight_resolver,
    )?;
    let mono_final = score_fixed_mono_consensus_candidate(
        "current_default",
        rendered,
        &mono_refs,
        &mono_target,
    )?
    .final_score;
    let side_targets = build_fixed_sectioned_side_consensus_with_weight_resolver(
        rendered,
        loaded_refs,
        SAMPLE_RATE as usize,
        weight_resolver,
    )?;
    let side_consensus_final =
        score_fixed_sectioned_side_consensus_candidate("current_default", rendered, &side_targets)?
            .final_score;
    let dynamics_targets = build_fixed_sectioned_side_dynamics_with_weight_resolver(
        rendered,
        loaded_refs,
        SAMPLE_RATE as usize,
        weight_resolver,
    )?;
    let hybrid_final = {
        let side_reliability = sectioned_side_transient_reliability(&side_targets);
        let mut side_scores = Vec::new();
        let mut side_weights = Vec::new();
        for reference in &mono_refs {
            let ref_samples = &loaded_refs.get(reference.ref_index)?.1;
            let metrics = score_fixed_trusted_window(rendered, ref_samples, reference.selection)?;
            side_scores.push(normalized_score(
                metrics.side,
                reference.selection.trusted_self_side,
            ));
            side_weights.push(reference.side_weight);
        }
        score_hybrid_mono_consensus_candidate(
            "current_default",
            score_fixed_mono_consensus_candidate(
                "current_default",
                rendered,
                &mono_refs,
                &mono_target,
            )?,
            &side_scores,
            &side_weights,
            side_reliability,
        )
        .final_score
    };
    let side_dynamics_final = score_fixed_sectioned_side_dynamics_candidate(
        "current_default",
        rendered,
        &dynamics_targets,
    )?
    .final_score;

    Some(OracleScoreSnapshot {
        mono_final,
        hybrid_final,
        side_consensus_final,
        side_dynamics_final,
    })
}

fn calibrated_default_without_side_eq() -> AudioOutputConfig {
    AudioOutputConfig::legacy()
        .with_gain(2.5)
        .with_ym_gain(1.1)
        .with_psg_gain(0.65)
        .with_stereo_crossfeed(0.35)
        .with_post_low_pass_hz(12_000.0)
        .with_post_eq_1(AudioEqStage::low_shelf(110.0, -6.0))
        .with_post_eq_2(AudioEqStage::peaking(380.0, 0.65, 4.5))
        .with_post_eq_3(AudioEqStage::high_shelf(2_600.0, -2.8))
        .with_post_eq_4(AudioEqStage::peaking(190.0, 0.90, 3.2))
        .with_post_eq_5(AudioEqStage::peaking(560.0, 1.20, -2.4))
}

fn consensus_profile_candidates() -> Vec<(&'static str, AudioOutputConfig)> {
    let current_default = AudioOutputConfig::default();
    vec![
        ("current_default", current_default),
        ("no_side_eq", calibrated_default_without_side_eq()),
        (
            "side_air_plus",
            current_default
                .with_post_side_eq_1(AudioEqStage::peaking(420.0, 0.95, -1.6))
                .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 1.6)),
        ),
        (
            "side_lowmid_cut_more",
            current_default
                .with_post_side_eq_1(AudioEqStage::peaking(420.0, 0.95, -2.6))
                .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 0.8)),
        ),
        ("side_gain_down", current_default.with_side_gain(0.9)),
        ("side_gain_up", current_default.with_side_gain(1.1)),
        ("xf_0_30", current_default.with_stereo_crossfeed(0.30)),
        ("xf_0_40", current_default.with_stereo_crossfeed(0.40)),
        ("psg_0_70", current_default.with_psg_gain(0.70)),
        ("psg_0_90", current_default.with_psg_gain(0.90)),
    ]
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }

    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for i in 0..n {
        let ax = f64::from(a[i]);
        let bx = f64::from(b[i]);
        dot += ax * bx;
        norm_a += ax * ax;
        norm_b += bx * bx;
    }

    if norm_a < 1e-12 || norm_b < 1e-12 {
        return 0.0;
    }

    (dot / (norm_a.sqrt() * norm_b.sqrt())) as f32
}

fn log_frequency_bins(
    frame_len: usize,
    sample_rate: u32,
    min_hz: f32,
    max_hz: f32,
    bands: usize,
) -> Vec<usize> {
    if frame_len < 2 || bands == 0 {
        return Vec::new();
    }

    let min_hz = min_hz.max(1.0);
    let max_hz = max_hz.min(sample_rate as f32 * 0.5 - 1.0).max(min_hz);
    let ln_min = min_hz.ln();
    let ln_max = max_hz.ln();

    let mut bins = Vec::with_capacity(bands);
    for idx in 0..bands {
        let t = if bands == 1 {
            0.0
        } else {
            idx as f32 / (bands - 1) as f32
        };
        let hz = (ln_min + (ln_max - ln_min) * t).exp();
        let bin = ((hz * frame_len as f32 / sample_rate as f32).round() as usize)
            .clamp(1, frame_len / 2 - 1);
        if bins.last().copied() != Some(bin) {
            bins.push(bin);
        }
    }

    bins
}

fn goertzel_power(samples: &[f32], bin: usize) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }

    let n = samples.len() as f32;
    let omega = 2.0 * std::f32::consts::PI * bin as f32 / n;
    let coeff = 2.0 * omega.cos();
    let mut s_prev = 0.0f32;
    let mut s_prev2 = 0.0f32;

    for (idx, &sample) in samples.iter().enumerate() {
        let window = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * idx as f32 / (n - 1.0)).cos();
        let s = sample * window + coeff * s_prev - s_prev2;
        s_prev2 = s_prev;
        s_prev = s;
    }

    (s_prev2 * s_prev2 + s_prev * s_prev - coeff * s_prev * s_prev2).max(0.0)
}

fn dft_bin(samples: &[f32], bin: usize) -> (f32, f32) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }

    let n = samples.len() as f32;
    let mut re = 0.0f32;
    let mut im = 0.0f32;
    for (idx, &sample) in samples.iter().enumerate() {
        let window = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * idx as f32 / (n - 1.0)).cos();
        let angle = 2.0 * std::f32::consts::PI * bin as f32 * idx as f32 / n;
        let sample = sample * window;
        re += sample * angle.cos();
        im -= sample * angle.sin();
    }
    (re, im)
}

fn average_phase_delta(
    a: &[f32],
    b: &[f32],
    frame_len: usize,
    hop_len: usize,
    bins: &[usize],
) -> Vec<(f32, f32)> {
    if frame_len == 0
        || hop_len == 0
        || bins.is_empty()
        || a.len() < frame_len
        || b.len() < frame_len
    {
        return vec![(0.0, 0.0); bins.len()];
    }

    let overlap = a.len().min(b.len());
    let mut phase_re = vec![0.0f64; bins.len()];
    let mut phase_im = vec![0.0f64; bins.len()];
    let mut weight_sum = vec![0.0f64; bins.len()];

    for start in (0..=overlap - frame_len).step_by(hop_len) {
        let frame_a = &a[start..start + frame_len];
        let frame_b = &b[start..start + frame_len];
        for (idx, &bin) in bins.iter().enumerate() {
            let (a_re, a_im) = dft_bin(frame_a, bin);
            let (b_re, b_im) = dft_bin(frame_b, bin);
            let mag_a = (a_re * a_re + a_im * a_im).sqrt();
            let mag_b = (b_re * b_re + b_im * b_im).sqrt();
            let weight = f64::from((mag_a * mag_b).sqrt());
            if weight < 1e-6 {
                continue;
            }

            let cross_re = a_re * b_re + a_im * b_im;
            let cross_im = a_re * b_im - a_im * b_re;
            let mag_cross = (cross_re * cross_re + cross_im * cross_im).sqrt();
            if mag_cross < 1e-9 {
                continue;
            }

            phase_re[idx] += f64::from(cross_re / mag_cross) * weight;
            phase_im[idx] += f64::from(cross_im / mag_cross) * weight;
            weight_sum[idx] += weight;
        }
    }

    phase_re
        .into_iter()
        .zip(phase_im)
        .zip(weight_sum)
        .map(|((re, im), weight)| {
            if weight < 1e-6 {
                return (0.0, 0.0);
            }
            let re = re / weight;
            let im = im / weight;
            let coherence = (re * re + im * im).sqrt().clamp(0.0, 1.0) as f32;
            let phase_deg = (im.atan2(re).to_degrees()) as f32;
            (phase_deg, coherence)
        })
        .collect()
}

fn mean_phase_coherence(phases: &[(f32, f32)]) -> f32 {
    if phases.is_empty() {
        return 0.0;
    }
    phases.iter().map(|&(_, coherence)| coherence).sum::<f32>() / phases.len() as f32
}

fn average_power_spectrum(
    samples: &[f32],
    frame_len: usize,
    hop_len: usize,
    bins: &[usize],
) -> Vec<f32> {
    if frame_len == 0 || hop_len == 0 || bins.is_empty() || samples.len() < frame_len {
        return vec![0.0; bins.len()];
    }

    let mut accum = vec![0.0f32; bins.len()];
    let mut frames = 0usize;

    for start in (0..=samples.len() - frame_len).step_by(hop_len) {
        let frame = &samples[start..start + frame_len];
        for (idx, &bin) in bins.iter().enumerate() {
            accum[idx] += goertzel_power(frame, bin);
        }
        frames += 1;
    }

    if frames > 0 {
        for value in &mut accum {
            *value /= frames as f32;
        }
    }

    accum
}

fn average_log_spectrum(
    samples: &[f32],
    frame_len: usize,
    hop_len: usize,
    bins: &[usize],
) -> Vec<f32> {
    if frame_len == 0 || hop_len == 0 || bins.is_empty() || samples.len() < frame_len {
        return vec![0.0; bins.len()];
    }

    let mut accum = vec![0.0f32; bins.len()];
    let mut frames = 0usize;

    for start in (0..=samples.len() - frame_len).step_by(hop_len) {
        let frame = &samples[start..start + frame_len];
        for (idx, &bin) in bins.iter().enumerate() {
            let power = goertzel_power(frame, bin);
            accum[idx] += (1.0 + power).ln();
        }
        frames += 1;
    }

    if frames > 0 {
        for value in &mut accum {
            *value /= frames as f32;
        }
    }

    let mean = accum.iter().copied().sum::<f32>() / accum.len().max(1) as f32;
    for value in &mut accum {
        *value -= mean;
    }

    accum
}

fn extract_mono_consensus_window(
    stereo_samples: &[f32],
    start: usize,
    len: usize,
    frame_len: usize,
    hop_len: usize,
    bins: &[usize],
    weight: f32,
) -> Option<MonoConsensusWindow> {
    if len < frame_len
        || start
            .checked_add(len)
            .is_none_or(|end| end > stereo_samples.len() / 2)
    {
        return None;
    }

    let stereo = stereo_window(stereo_samples, start, len);
    let (mid, _) = stereo_mid_side(stereo);
    Some(MonoConsensusWindow {
        spectrum: average_log_spectrum(&mid, frame_len, hop_len, bins),
        rms: rms(&mid),
        weight,
    })
}

fn extract_side_consensus_window(
    stereo_samples: &[f32],
    start: usize,
    len: usize,
    frame_len: usize,
    hop_len: usize,
    bins: &[usize],
    weight: f32,
) -> Option<SideConsensusWindow> {
    if len < frame_len
        || start
            .checked_add(len)
            .is_none_or(|end| end > stereo_samples.len() / 2)
    {
        return None;
    }

    let stereo = stereo_window(stereo_samples, start, len);
    let (_, side) = stereo_mid_side(stereo);
    Some(SideConsensusWindow {
        spectrum: average_log_spectrum(&side, frame_len, hop_len, bins),
        ratio: stereo_side_ratio(stereo),
        weight,
    })
}

fn extract_side_dynamics_window(
    stereo_samples: &[f32],
    start: usize,
    len: usize,
    env_window: usize,
    weight: f32,
) -> Option<SideDynamicsWindow> {
    if len < env_window.saturating_mul(4)
        || start
            .checked_add(len)
            .is_none_or(|end| end > stereo_samples.len() / 2)
    {
        return None;
    }

    let stereo = stereo_window(stereo_samples, start, len);
    let (_, side) = stereo_mid_side(stereo);
    let envelope = rms_envelope(&side, env_window);
    if envelope.len() < 4 {
        return None;
    }

    Some(SideDynamicsWindow {
        envelope,
        ratio: stereo_side_ratio(stereo),
        weight,
    })
}

fn aggregate_side_consensus_windows(
    windows: &[SideConsensusWindow],
) -> Option<SideConsensusWindow> {
    if windows.is_empty() {
        return None;
    }

    let spectra: Vec<Vec<f32>> = windows
        .iter()
        .map(|window| window.spectrum.clone())
        .collect();
    let weights: Vec<f32> = windows.iter().map(|window| window.weight).collect();
    let spectrum = weighted_average_vectors(&spectra, &weights);
    if spectrum.is_empty() {
        return None;
    }

    Some(SideConsensusWindow {
        spectrum,
        ratio: weighted_average(
            &windows
                .iter()
                .map(|window| window.ratio)
                .collect::<Vec<_>>(),
            &weights,
        ),
        weight: weights.iter().sum(),
    })
}

fn aggregate_side_dynamics_windows(windows: &[SideDynamicsWindow]) -> Option<SideDynamicsWindow> {
    if windows.is_empty() {
        return None;
    }

    let envelopes: Vec<Vec<f32>> = windows
        .iter()
        .map(|window| window.envelope.clone())
        .collect();
    let weights: Vec<f32> = windows.iter().map(|window| window.weight).collect();
    let envelope = weighted_average_vectors(&envelopes, &weights);
    if envelope.is_empty() {
        return None;
    }

    Some(SideDynamicsWindow {
        envelope,
        ratio: weighted_average(
            &windows
                .iter()
                .map(|window| window.ratio)
                .collect::<Vec<_>>(),
            &weights,
        ),
        weight: weights.iter().sum(),
    })
}

fn stereo_log_spectra(
    stereo: &[f32],
    frame_len: usize,
    hop_len: usize,
    bins: &[usize],
) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    let (left, _) = stereo_left_right(stereo);
    let (mid, side) = stereo_mid_side(stereo);
    (
        average_log_spectrum(&left, frame_len, hop_len, bins),
        average_log_spectrum(&mid, frame_len, hop_len, bins),
        average_log_spectrum(&side, frame_len, hop_len, bins),
    )
}

fn weighted_average_vectors(vectors: &[Vec<f32>], weights: &[f32]) -> Vec<f32> {
    if vectors.is_empty() || vectors.len() != weights.len() {
        return Vec::new();
    }
    let len = vectors[0].len();
    if vectors.iter().any(|v| v.len() != len) {
        return Vec::new();
    }

    let weight_sum: f32 = weights.iter().sum();
    if weight_sum <= 1e-6 {
        return vec![0.0; len];
    }

    let mut output = vec![0.0; len];
    for (vector, &weight) in vectors.iter().zip(weights) {
        for (idx, &value) in vector.iter().enumerate() {
            output[idx] += value * weight;
        }
    }
    for value in &mut output {
        *value /= weight_sum;
    }
    output
}

fn build_mono_consensus_target(windows: &[MonoConsensusWindow]) -> Option<MonoConsensusTarget> {
    if windows.is_empty() {
        return None;
    }

    let spectra: Vec<Vec<f32>> = windows
        .iter()
        .map(|window| window.spectrum.clone())
        .collect();
    let weights: Vec<f32> = windows.iter().map(|window| window.weight).collect();
    let spectrum = weighted_average_vectors(&spectra, &weights);
    if spectrum.is_empty() {
        return None;
    }
    let rms_values: Vec<f32> = windows.iter().map(|window| window.rms).collect();
    let rms = weighted_average(&rms_values, &weights);
    let self_spectral_values: Vec<f32> = windows
        .iter()
        .map(|window| cosine_similarity(&window.spectrum, &spectrum))
        .collect();
    let self_rms_values: Vec<f32> = windows
        .iter()
        .map(|window| rms_fit_score(window.rms / rms.max(1e-9)))
        .collect();

    Some(MonoConsensusTarget {
        spectrum,
        rms,
        self_spectral: weighted_average(&self_spectral_values, &weights),
        self_rms_fit: weighted_average(&self_rms_values, &weights),
    })
}

fn side_ratio_fit_score(ratio: f32, target_ratio: f32) -> f32 {
    1.0 / (1.0 + (ratio - target_ratio).abs() * 4.0)
}

fn build_side_consensus_target(windows: &[SideConsensusWindow]) -> Option<SideConsensusTarget> {
    if windows.is_empty() {
        return None;
    }

    let spectra: Vec<Vec<f32>> = windows
        .iter()
        .map(|window| window.spectrum.clone())
        .collect();
    let base_weights: Vec<f32> = windows.iter().map(|window| window.weight).collect();
    let prelim_spectrum = weighted_average_vectors(&spectra, &base_weights);
    if prelim_spectrum.is_empty() {
        return None;
    }

    let ratios: Vec<f32> = windows.iter().map(|window| window.ratio).collect();
    let prelim_ratio = weighted_average(&ratios, &base_weights);
    let agreement_weights: Vec<f32> = windows
        .iter()
        .map(|window| {
            let spectral_agreement = cosine_similarity(&window.spectrum, &prelim_spectrum);
            let ratio_agreement = side_ratio_fit_score(window.ratio, prelim_ratio);
            let agreement = (spectral_agreement * 0.85 + ratio_agreement * 0.15).clamp(0.0, 1.0);
            window.weight * agreement * agreement
        })
        .collect();

    let spectrum = weighted_average_vectors(&spectra, &agreement_weights);
    if spectrum.is_empty() {
        return None;
    }
    let ratio = weighted_average(&ratios, &agreement_weights);
    let self_spectral_values: Vec<f32> = windows
        .iter()
        .map(|window| cosine_similarity(&window.spectrum, &spectrum))
        .collect();
    let self_ratio_values: Vec<f32> = windows
        .iter()
        .map(|window| side_ratio_fit_score(window.ratio, ratio))
        .collect();

    Some(SideConsensusTarget {
        spectrum,
        ratio,
        self_spectral: weighted_average(&self_spectral_values, &agreement_weights),
        self_ratio_fit: weighted_average(&self_ratio_values, &agreement_weights),
    })
}

fn build_side_dynamics_target(windows: &[SideDynamicsWindow]) -> Option<SideDynamicsTarget> {
    if windows.is_empty() {
        return None;
    }

    let envelopes: Vec<Vec<f32>> = windows
        .iter()
        .map(|window| window.envelope.clone())
        .collect();
    let base_weights: Vec<f32> = windows.iter().map(|window| window.weight).collect();
    let prelim_envelope = weighted_average_vectors(&envelopes, &base_weights);
    if prelim_envelope.is_empty() {
        return None;
    }

    let ratios: Vec<f32> = windows.iter().map(|window| window.ratio).collect();
    let prelim_ratio = weighted_average(&ratios, &base_weights);
    let agreement_weights: Vec<f32> = windows
        .iter()
        .map(|window| {
            let envelope_agreement =
                cross_correlation(&window.envelope, &prelim_envelope).clamp(0.0, 1.0);
            let ratio_agreement = side_ratio_fit_score(window.ratio, prelim_ratio);
            let agreement = (envelope_agreement * 0.80 + ratio_agreement * 0.20).clamp(0.0, 1.0);
            window.weight * agreement * agreement
        })
        .collect();

    let envelope = weighted_average_vectors(&envelopes, &agreement_weights);
    if envelope.is_empty() {
        return None;
    }
    let ratio = weighted_average(&ratios, &agreement_weights);
    let self_envelope_values: Vec<f32> = windows
        .iter()
        .map(|window| cross_correlation(&window.envelope, &envelope).clamp(0.0, 1.0))
        .collect();
    let self_ratio_values: Vec<f32> = windows
        .iter()
        .map(|window| side_ratio_fit_score(window.ratio, ratio))
        .collect();

    Some(SideDynamicsTarget {
        envelope,
        ratio,
        self_envelope: weighted_average(&self_envelope_values, &agreement_weights),
        self_ratio_fit: weighted_average(&self_ratio_values, &agreement_weights),
    })
}

fn score_mono_consensus_candidate_windows(
    name: &str,
    windows: &[MonoConsensusWindow],
    target: &MonoConsensusTarget,
) -> ConsensusCandidateScore {
    if windows.is_empty() {
        return ConsensusCandidateScore {
            name: name.to_owned(),
            average_score: 0.0,
            disagreement_penalty: 0.0,
            final_score: 0.0,
        };
    }

    let weights: Vec<f32> = windows.iter().map(|window| window.weight).collect();
    let spectral_scores: Vec<f32> = windows
        .iter()
        .map(|window| {
            normalized_score(
                cosine_similarity(&window.spectrum, &target.spectrum),
                target.self_spectral,
            )
        })
        .collect();
    let rms_scores: Vec<f32> = windows
        .iter()
        .map(|window| {
            normalized_score(
                rms_fit_score(window.rms / target.rms.max(1e-9)),
                target.self_rms_fit,
            )
        })
        .collect();
    let combined_scores: Vec<f32> = spectral_scores
        .iter()
        .zip(&rms_scores)
        .map(|(&spectral, &rms)| spectral * 0.85 + rms * 0.15)
        .collect();

    let average_score = weighted_average(&combined_scores, &weights);
    let disagreement_penalty = weighted_mean_absolute_deviation(&combined_scores, &weights) * 0.70
        + weighted_mean_absolute_deviation(&spectral_scores, &weights) * 0.30;

    ConsensusCandidateScore {
        name: name.to_owned(),
        average_score,
        disagreement_penalty,
        final_score: average_score - disagreement_penalty,
    }
}

fn score_side_consensus_candidate_windows(
    name: &str,
    windows: &[SideConsensusWindow],
    target: &SideConsensusTarget,
) -> ConsensusCandidateScore {
    if windows.is_empty() {
        return ConsensusCandidateScore {
            name: name.to_owned(),
            average_score: 0.0,
            disagreement_penalty: 0.0,
            final_score: 0.0,
        };
    }

    let weights: Vec<f32> = windows.iter().map(|window| window.weight).collect();
    let spectral_scores: Vec<f32> = windows
        .iter()
        .map(|window| {
            normalized_score(
                cosine_similarity(&window.spectrum, &target.spectrum),
                target.self_spectral,
            )
        })
        .collect();
    let ratio_scores: Vec<f32> = windows
        .iter()
        .map(|window| {
            normalized_score(
                side_ratio_fit_score(window.ratio, target.ratio),
                target.self_ratio_fit,
            )
        })
        .collect();
    let combined_scores: Vec<f32> = spectral_scores
        .iter()
        .zip(&ratio_scores)
        .map(|(&spectral, &ratio)| spectral * 0.85 + ratio * 0.15)
        .collect();

    let average_score = weighted_average(&combined_scores, &weights);
    let disagreement_penalty = weighted_mean_absolute_deviation(&combined_scores, &weights) * 0.65
        + weighted_mean_absolute_deviation(&spectral_scores, &weights) * 0.25
        + weighted_mean_absolute_deviation(&ratio_scores, &weights) * 0.10;

    ConsensusCandidateScore {
        name: name.to_owned(),
        average_score,
        disagreement_penalty,
        final_score: average_score - disagreement_penalty,
    }
}

fn score_side_dynamics_candidate_windows(
    name: &str,
    windows: &[SideDynamicsWindow],
    target: &SideDynamicsTarget,
) -> ConsensusCandidateScore {
    if windows.is_empty() {
        return ConsensusCandidateScore {
            name: name.to_owned(),
            average_score: 0.0,
            disagreement_penalty: 0.0,
            final_score: 0.0,
        };
    }

    let weights: Vec<f32> = windows.iter().map(|window| window.weight).collect();
    let envelope_scores: Vec<f32> = windows
        .iter()
        .map(|window| {
            normalized_score(
                cross_correlation(&window.envelope, &target.envelope).clamp(0.0, 1.0),
                target.self_envelope,
            )
        })
        .collect();
    let ratio_scores: Vec<f32> = windows
        .iter()
        .map(|window| {
            normalized_score(
                side_ratio_fit_score(window.ratio, target.ratio),
                target.self_ratio_fit,
            )
        })
        .collect();
    let combined_scores: Vec<f32> = envelope_scores
        .iter()
        .zip(&ratio_scores)
        .map(|(&envelope, &ratio)| envelope * 0.80 + ratio * 0.20)
        .collect();

    let average_score = weighted_average(&combined_scores, &weights);
    let disagreement_penalty = weighted_mean_absolute_deviation(&combined_scores, &weights) * 0.70
        + weighted_mean_absolute_deviation(&envelope_scores, &weights) * 0.20
        + weighted_mean_absolute_deviation(&ratio_scores, &weights) * 0.10;

    ConsensusCandidateScore {
        name: name.to_owned(),
        average_score,
        disagreement_penalty,
        final_score: average_score - disagreement_penalty,
    }
}

fn build_fixed_sectioned_side_consensus_with_weight_resolver<F>(
    anchor_rendered: &[f32],
    loaded_refs: &[(std::path::PathBuf, Vec<f32>)],
    section_len: usize,
    mut weight_resolver: F,
) -> Option<Vec<FixedSideConsensusSectionTarget>>
where
    F: FnMut(&Path) -> GhzReferenceWeight,
{
    let frame_len = 2048usize;
    let hop_len = 1024usize;
    let spectral_bins = log_frequency_bins(frame_len, SAMPLE_RATE, 80.0, 12_000.0, 24);

    let mut fixed_refs = Vec::new();
    let mut common_len = usize::MAX;
    for (ref_index, (path, ref_samples)) in loaded_refs.iter().enumerate() {
        let selection = select_trusted_reference_window(anchor_rendered, ref_samples)?;
        common_len = common_len.min(selection.trusted_len);
        let manual_weight = weight_resolver(path);
        fixed_refs.push((
            ref_index,
            selection,
            trusted_window_reliability_weight(selection) * manual_weight.side,
        ));
    }

    if fixed_refs.len() < 2 || common_len < frame_len {
        return None;
    }

    let mut section_targets = Vec::new();
    for (start, len) in partition_window_sections(common_len, section_len) {
        if len < frame_len {
            continue;
        }

        let mut windows = Vec::new();
        let mut dynamics_windows = Vec::new();
        let mut refs_meta = Vec::new();
        for (ref_index, selection, base_weight) in &fixed_refs {
            let ref_samples = &loaded_refs[*ref_index].1;
            let self_window = reference_self_window_at_offset(
                ref_samples,
                selection.trusted_prior_start + start,
                selection.trusted_ref_start + start,
                len,
                frame_len,
                hop_len,
                &spectral_bins,
            )?;
            let weight = *base_weight * side_section_consistency_weight(self_window);
            let window = extract_side_consensus_window(
                ref_samples,
                selection.trusted_ref_start + start,
                len,
                frame_len,
                hop_len,
                &spectral_bins,
                weight,
            )?;
            windows.push(window);
            if let Some(dynamics_window) = extract_side_dynamics_window(
                ref_samples,
                selection.trusted_ref_start + start,
                len,
                SIDE_DYNAMICS_ENV_WINDOW,
                weight,
            ) {
                dynamics_windows.push(dynamics_window);
            }
            refs_meta.push(FixedSideConsensusSectionRef {
                ref_index: *ref_index,
                ref_start: selection.trusted_ref_start + start,
                emu_start: selection.trusted_emu_start + start,
                weight,
            });
        }

        if windows.len() < 2 {
            continue;
        }

        let target = build_side_consensus_target(&windows)?;
        let transient_reliability = summarize_side_dynamics_window_consistency(
            &dynamics_windows,
            SIDE_DYNAMICS_MAX_LAG_BINS,
        )
        .map(reference_side_transient_consistency_weight)
        .unwrap_or(1.0);
        let window_weights: Vec<f32> = windows.iter().map(|window| window.weight).collect();
        let uniform = vec![1.0; window_weights.len()];
        let weight = weighted_average(&window_weights, &uniform)
            * (target.self_spectral * 0.85 + target.self_ratio_fit * 0.15)
            * transient_reliability;
        section_targets.push(FixedSideConsensusSectionTarget {
            start,
            len,
            target,
            refs: refs_meta,
            weight,
            transient_reliability,
        });
    }

    if section_targets.is_empty() {
        return None;
    }

    Some(section_targets)
}

fn build_fixed_sectioned_side_consensus(
    anchor_rendered: &[f32],
    loaded_refs: &[(std::path::PathBuf, Vec<f32>)],
    section_len: usize,
) -> Option<Vec<FixedSideConsensusSectionTarget>> {
    build_fixed_sectioned_side_consensus_with_weight_resolver(
        anchor_rendered,
        loaded_refs,
        section_len,
        |path| ghz_reference_weight(path),
    )
}

fn build_fixed_sectioned_side_dynamics_with_weight_resolver<F>(
    anchor_rendered: &[f32],
    loaded_refs: &[(std::path::PathBuf, Vec<f32>)],
    section_len: usize,
    mut weight_resolver: F,
) -> Option<Vec<FixedSideDynamicsSectionTarget>>
where
    F: FnMut(&Path) -> GhzReferenceWeight,
{
    let spectral_bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 12_000.0, 24);
    let mut fixed_refs = Vec::new();
    let mut common_len = usize::MAX;
    for (ref_index, (path, ref_samples)) in loaded_refs.iter().enumerate() {
        let selection = select_trusted_reference_window(anchor_rendered, ref_samples)?;
        common_len = common_len.min(selection.trusted_len);
        let manual_weight = weight_resolver(path);
        fixed_refs.push((
            ref_index,
            selection,
            trusted_window_reliability_weight(selection) * manual_weight.side,
        ));
    }

    if fixed_refs.len() < 2 || common_len < SIDE_DYNAMICS_ENV_WINDOW.saturating_mul(4) {
        return None;
    }

    let mut section_targets = Vec::new();
    for (start, len) in partition_window_sections(common_len, section_len) {
        if len < SIDE_DYNAMICS_ENV_WINDOW.saturating_mul(4) {
            continue;
        }

        let mut windows = Vec::new();
        let mut refs_meta = Vec::new();
        for (ref_index, selection, base_weight) in &fixed_refs {
            let ref_samples = &loaded_refs[*ref_index].1;
            let self_window = reference_self_window_at_offset(
                ref_samples,
                selection.trusted_prior_start + start,
                selection.trusted_ref_start + start,
                len,
                2048,
                1024,
                &spectral_bins,
            )?;
            let weight = *base_weight * side_section_consistency_weight(self_window);
            let window = extract_side_dynamics_window(
                ref_samples,
                selection.trusted_ref_start + start,
                len,
                SIDE_DYNAMICS_ENV_WINDOW,
                weight,
            )?;
            windows.push(window);
            refs_meta.push(FixedSideConsensusSectionRef {
                ref_index: *ref_index,
                ref_start: selection.trusted_ref_start + start,
                emu_start: selection.trusted_emu_start + start,
                weight,
            });
        }

        if windows.len() < 2 {
            continue;
        }

        let target = build_side_dynamics_target(&windows)?;
        let transient_consistency =
            summarize_side_dynamics_window_consistency(&windows, SIDE_DYNAMICS_MAX_LAG_BINS)?;
        let transient_weight = reference_side_transient_consistency_weight(transient_consistency);
        let window_weights: Vec<f32> = windows.iter().map(|window| window.weight).collect();
        let uniform = vec![1.0; window_weights.len()];
        let weight = weighted_average(&window_weights, &uniform)
            * (target.self_envelope * 0.80 + target.self_ratio_fit * 0.20)
            * transient_weight;
        section_targets.push(FixedSideDynamicsSectionTarget {
            start,
            len,
            target,
            refs: refs_meta,
            weight,
        });
    }

    if section_targets.is_empty() {
        return None;
    }

    Some(section_targets)
}

fn build_fixed_sectioned_side_dynamics(
    anchor_rendered: &[f32],
    loaded_refs: &[(std::path::PathBuf, Vec<f32>)],
    section_len: usize,
) -> Option<Vec<FixedSideDynamicsSectionTarget>> {
    build_fixed_sectioned_side_dynamics_with_weight_resolver(
        anchor_rendered,
        loaded_refs,
        section_len,
        |path| ghz_reference_weight(path),
    )
}

fn score_fixed_sectioned_side_consensus_candidate(
    name: &str,
    rendered: &[f32],
    sections: &[FixedSideConsensusSectionTarget],
) -> Option<SectionedSideConsensusScore> {
    let analyzed =
        analyze_fixed_sectioned_side_consensus_candidate_sections(name, rendered, sections)?;

    let mut section_averages = Vec::new();
    let mut section_penalties = Vec::new();
    let mut section_finals = Vec::new();
    let mut section_weights = Vec::new();
    for (section, analyzed) in sections.iter().zip(&analyzed) {
        section_averages.push(analyzed.average_score);
        section_penalties.push(analyzed.disagreement_penalty);
        section_finals.push(analyzed.final_score);
        section_weights.push(section.weight);
    }

    if section_averages.is_empty() {
        return None;
    }

    let average_score = weighted_average(&section_averages, &section_weights);
    let disagreement_penalty = weighted_average(&section_penalties, &section_weights)
        + weighted_mean_absolute_deviation(&section_finals, &section_weights) * 0.25;
    let final_score = average_score - disagreement_penalty;
    let worst_section_score = section_finals.iter().copied().fold(f32::INFINITY, f32::min);
    let dominant_section_impact = dominant_section_impact_index(&section_finals, &section_weights)
        .map(|(_, impact)| impact)
        .unwrap_or(0.0);

    Some(SectionedSideConsensusScore {
        average_score,
        disagreement_penalty,
        final_score,
        worst_section_score,
        dominant_section_impact,
    })
}

fn analyze_fixed_sectioned_side_consensus_candidate_sections(
    name: &str,
    rendered: &[f32],
    sections: &[FixedSideConsensusSectionTarget],
) -> Option<Vec<AnalyzedSideConsensusSection>> {
    let frame_len = 2048usize;
    let hop_len = 1024usize;
    let spectral_bins = log_frequency_bins(frame_len, SAMPLE_RATE, 80.0, 12_000.0, 24);

    let mut analyzed = Vec::new();
    for section in sections {
        let mut windows = Vec::new();
        for reference in &section.refs {
            let window = extract_side_consensus_window(
                rendered,
                reference.emu_start,
                section.len,
                frame_len,
                hop_len,
                &spectral_bins,
                reference.weight,
            )?;
            windows.push(window);
        }
        if windows.len() < 2 {
            continue;
        }

        let score = score_side_consensus_candidate_windows(name, &windows, &section.target);
        let candidate = aggregate_side_consensus_windows(&windows)?;
        analyzed.push(AnalyzedSideConsensusSection {
            average_score: score.average_score,
            disagreement_penalty: score.disagreement_penalty,
            final_score: score.final_score,
            candidate,
        });
    }

    if analyzed.is_empty() {
        return None;
    }

    Some(analyzed)
}

fn score_fixed_sectioned_side_dynamics_candidate(
    name: &str,
    rendered: &[f32],
    sections: &[FixedSideDynamicsSectionTarget],
) -> Option<SectionedSideDynamicsScore> {
    let analyzed =
        analyze_fixed_sectioned_side_dynamics_candidate_sections(name, rendered, sections)?;

    let mut section_averages = Vec::new();
    let mut section_penalties = Vec::new();
    let mut section_finals = Vec::new();
    let mut section_weights = Vec::new();
    for (section, analyzed) in sections.iter().zip(&analyzed) {
        section_averages.push(analyzed.average_score);
        section_penalties.push(analyzed.disagreement_penalty);
        section_finals.push(analyzed.final_score);
        section_weights.push(section.weight);
    }

    if section_averages.is_empty() {
        return None;
    }

    let average_score = weighted_average(&section_averages, &section_weights);
    let disagreement_penalty = weighted_average(&section_penalties, &section_weights)
        + weighted_mean_absolute_deviation(&section_finals, &section_weights) * 0.25;
    let final_score = average_score - disagreement_penalty;
    let worst_section_score = section_finals.iter().copied().fold(f32::INFINITY, f32::min);
    let dominant_section_impact = dominant_section_impact_index(&section_finals, &section_weights)
        .map(|(_, impact)| impact)
        .unwrap_or(0.0);

    Some(SectionedSideDynamicsScore {
        average_score,
        disagreement_penalty,
        final_score,
        worst_section_score,
        dominant_section_impact,
    })
}

fn analyze_fixed_sectioned_side_dynamics_candidate_sections(
    name: &str,
    rendered: &[f32],
    sections: &[FixedSideDynamicsSectionTarget],
) -> Option<Vec<AnalyzedSideDynamicsSection>> {
    let mut analyzed = Vec::new();
    for section in sections {
        let mut windows = Vec::new();
        for reference in &section.refs {
            let window = extract_side_dynamics_window(
                rendered,
                reference.emu_start,
                section.len,
                SIDE_DYNAMICS_ENV_WINDOW,
                reference.weight,
            )?;
            windows.push(window);
        }
        if windows.len() < 2 {
            continue;
        }

        let score = score_side_dynamics_candidate_windows(name, &windows, &section.target);
        let candidate = aggregate_side_dynamics_windows(&windows)?;
        analyzed.push(AnalyzedSideDynamicsSection {
            average_score: score.average_score,
            disagreement_penalty: score.disagreement_penalty,
            final_score: score.final_score,
            candidate,
        });
    }

    if analyzed.is_empty() {
        return None;
    }

    Some(analyzed)
}

/// Find the best lag between two short envelope sequences.
/// Positive offset means the candidate starts later than the target.
fn best_envelope_offset_bins(
    candidate: &[f32],
    target: &[f32],
    max_offset_bins: usize,
) -> Option<(isize, f32)> {
    let n = candidate.len().min(target.len());
    if n < 4 {
        return None;
    }

    let mut offsets = Vec::with_capacity(max_offset_bins * 2 + 1);
    offsets.push(0isize);
    for delta in 1..=max_offset_bins as isize {
        offsets.push(delta);
        offsets.push(-delta);
    }

    let mut best: Option<(isize, f32)> = None;
    for offset in offsets {
        let (candidate_start, target_start) = if offset >= 0 {
            (offset as usize, 0usize)
        } else {
            (0usize, (-offset) as usize)
        };
        let overlap = n
            .saturating_sub(candidate_start)
            .min(n.saturating_sub(target_start));
        if overlap < 4 {
            continue;
        }

        let corr = cross_correlation(
            &candidate[candidate_start..candidate_start + overlap],
            &target[target_start..target_start + overlap],
        );
        if best.is_none_or(|(_, best_corr)| corr > best_corr) {
            best = Some((offset, corr));
        }
    }

    best
}

fn correlation_at_offset_bins(candidate: &[f32], target: &[f32], offset: isize) -> f32 {
    let n = candidate.len().min(target.len());
    if n < 4 {
        return 0.0;
    }

    let (candidate_start, target_start) = if offset >= 0 {
        (offset as usize, 0usize)
    } else {
        (0usize, (-offset) as usize)
    };
    let overlap = n
        .saturating_sub(candidate_start)
        .min(n.saturating_sub(target_start));
    if overlap < 4 {
        return 0.0;
    }

    cross_correlation(
        &candidate[candidate_start..candidate_start + overlap],
        &target[target_start..target_start + overlap],
    )
}

fn analyze_transient_alignment(
    candidate: &[f32],
    target: &[f32],
    max_lag_bins: usize,
) -> Option<TransientAlignmentMetrics> {
    let n = candidate.len().min(target.len());
    if n < 4 {
        return None;
    }

    let envelope_corr = cross_correlation(candidate, target);
    let (lag_bins, adjusted_envelope_corr) =
        best_envelope_offset_bins(candidate, target, max_lag_bins).unwrap_or((0, envelope_corr));
    let candidate_transient = envelope_transient_profile(candidate);
    let target_transient = envelope_transient_profile(target);
    if candidate_transient.len().min(target_transient.len()) < 4 {
        return None;
    }

    let transient_corr = cross_correlation(&candidate_transient, &target_transient);
    let adjusted_transient_corr =
        correlation_at_offset_bins(&candidate_transient, &target_transient, lag_bins);
    let transient_rms_ratio = rms(&candidate_transient) / rms(&target_transient).max(1e-9);

    Some(TransientAlignmentMetrics {
        envelope_corr,
        lag_bins,
        adjusted_envelope_corr,
        transient_corr,
        adjusted_transient_corr,
        transient_rms_ratio,
    })
}

fn analyze_reference_side_dynamics_pairs(
    loaded_refs: &[(std::path::PathBuf, Vec<f32>)],
    section: &FixedSideDynamicsSectionTarget,
) -> Option<Vec<ReferenceSideDynamicsPairAnalysis>> {
    let mut windows = Vec::new();
    for reference in &section.refs {
        let (path, samples) = loaded_refs.get(reference.ref_index)?;
        let window = extract_side_dynamics_window(
            samples,
            reference.ref_start,
            section.len,
            SIDE_DYNAMICS_ENV_WINDOW,
            reference.weight,
        )?;
        windows.push((
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            window,
        ));
    }
    if windows.len() < 2 {
        return None;
    }

    let mut pairs = Vec::new();
    for left_idx in 0..windows.len() {
        for right_idx in left_idx + 1..windows.len() {
            let (left_name, left_window) = &windows[left_idx];
            let (right_name, right_window) = &windows[right_idx];
            let metrics = analyze_transient_alignment(
                &left_window.envelope,
                &right_window.envelope,
                SIDE_DYNAMICS_MAX_LAG_BINS,
            )?;
            pairs.push(ReferenceSideDynamicsPairAnalysis {
                left_name: left_name.clone(),
                right_name: right_name.clone(),
                weight: (left_window.weight + right_window.weight) * 0.5,
                left_ratio: left_window.ratio,
                right_ratio: right_window.ratio,
                metrics,
            });
        }
    }

    if pairs.is_empty() {
        return None;
    }

    Some(pairs)
}

fn summarize_reference_side_dynamics_pairs(
    pairs: &[ReferenceSideDynamicsPairAnalysis],
) -> Option<ReferenceSideDynamicsConsistency> {
    if pairs.is_empty() {
        return None;
    }

    let weights: Vec<f32> = pairs.iter().map(|pair| pair.weight).collect();
    let average_envelope_corr = weighted_average(
        &pairs
            .iter()
            .map(|pair| pair.metrics.envelope_corr)
            .collect::<Vec<_>>(),
        &weights,
    );
    let average_adjusted_envelope_corr = weighted_average(
        &pairs
            .iter()
            .map(|pair| pair.metrics.adjusted_envelope_corr)
            .collect::<Vec<_>>(),
        &weights,
    );
    let average_transient_corr = weighted_average(
        &pairs
            .iter()
            .map(|pair| pair.metrics.transient_corr)
            .collect::<Vec<_>>(),
        &weights,
    );
    let average_adjusted_transient_corr = weighted_average(
        &pairs
            .iter()
            .map(|pair| pair.metrics.adjusted_transient_corr)
            .collect::<Vec<_>>(),
        &weights,
    );
    let average_transient_rms_fit = weighted_average(
        &pairs
            .iter()
            .map(|pair| rms_fit_score(pair.metrics.transient_rms_ratio))
            .collect::<Vec<_>>(),
        &weights,
    );
    let average_abs_lag_bins = weighted_average(
        &pairs
            .iter()
            .map(|pair| pair.metrics.lag_bins.unsigned_abs() as f32)
            .collect::<Vec<_>>(),
        &weights,
    );
    let worst_adjusted_transient_corr = pairs
        .iter()
        .map(|pair| pair.metrics.adjusted_transient_corr)
        .fold(f32::INFINITY, f32::min);

    Some(ReferenceSideDynamicsConsistency {
        pair_count: pairs.len(),
        average_envelope_corr,
        average_adjusted_envelope_corr,
        average_transient_corr,
        average_adjusted_transient_corr,
        average_transient_rms_fit,
        average_abs_lag_bins,
        worst_adjusted_transient_corr,
    })
}

fn summarize_reference_side_consensus_pairs(
    pairs: &[ReferenceSideConsensusPairAnalysis],
) -> Option<ReferenceSideConsensusConsistency> {
    if pairs.is_empty() {
        return None;
    }

    let weights: Vec<f32> = pairs.iter().map(|pair| pair.weight).collect();
    let average_spectral_similarity = weighted_average(
        &pairs
            .iter()
            .map(|pair| pair.spectral_similarity)
            .collect::<Vec<_>>(),
        &weights,
    );
    let average_ratio_fit = weighted_average(
        &pairs.iter().map(|pair| pair.ratio_fit).collect::<Vec<_>>(),
        &weights,
    );
    let worst_spectral_similarity = pairs
        .iter()
        .map(|pair| pair.spectral_similarity)
        .fold(f32::INFINITY, f32::min);

    Some(ReferenceSideConsensusConsistency {
        pair_count: pairs.len(),
        average_spectral_similarity,
        average_ratio_fit,
        worst_spectral_similarity,
    })
}

fn summarize_fixed_side_consensus_section_refs(
    section: &FixedSideConsensusSectionTarget,
    loaded_refs: &[(std::path::PathBuf, Vec<f32>)],
) -> Option<ReferenceSideConsensusConsistency> {
    if section.refs.len() < 2 {
        return None;
    }

    let spectral_bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 12_000.0, 24);
    let mut windows = Vec::new();
    for reference in &section.refs {
        let (path, ref_samples) = loaded_refs.get(reference.ref_index)?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("reference")
            .to_owned();
        let window = extract_side_consensus_window(
            ref_samples,
            reference.ref_start,
            section.len,
            2048,
            1024,
            &spectral_bins,
            reference.weight,
        )?;
        windows.push((name, window));
    }

    let mut pairs = Vec::new();
    for left_idx in 0..windows.len() {
        for right_idx in left_idx + 1..windows.len() {
            let (_left_name, left_window) = &windows[left_idx];
            let (_right_name, right_window) = &windows[right_idx];
            pairs.push(ReferenceSideConsensusPairAnalysis {
                weight: (left_window.weight + right_window.weight) * 0.5,
                spectral_similarity: cosine_similarity(
                    &left_window.spectrum,
                    &right_window.spectrum,
                ),
                ratio_fit: side_ratio_fit_score(left_window.ratio, right_window.ratio),
            });
        }
    }

    summarize_reference_side_consensus_pairs(&pairs)
}

fn summarize_fixed_side_dynamics_section_refs(
    section: &FixedSideDynamicsSectionTarget,
    loaded_refs: &[(std::path::PathBuf, Vec<f32>)],
) -> Option<ReferenceSideDynamicsConsistency> {
    if section.refs.len() < 2 {
        return None;
    }

    let mut windows = Vec::new();
    for reference in &section.refs {
        let (path, ref_samples) = loaded_refs.get(reference.ref_index)?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("reference")
            .to_owned();
        let window = extract_side_dynamics_window(
            ref_samples,
            reference.ref_start,
            section.len,
            SIDE_DYNAMICS_ENV_WINDOW,
            reference.weight,
        )?;
        windows.push((name, window));
    }

    let mut pairs = Vec::new();
    for left_idx in 0..windows.len() {
        for right_idx in left_idx + 1..windows.len() {
            let (left_name, left_window) = &windows[left_idx];
            let (right_name, right_window) = &windows[right_idx];
            let metrics = analyze_transient_alignment(
                &left_window.envelope,
                &right_window.envelope,
                SIDE_DYNAMICS_MAX_LAG_BINS,
            )?;
            pairs.push(ReferenceSideDynamicsPairAnalysis {
                left_name: left_name.clone(),
                right_name: right_name.clone(),
                weight: (left_window.weight + right_window.weight) * 0.5,
                left_ratio: left_window.ratio,
                right_ratio: right_window.ratio,
                metrics,
            });
        }
    }

    summarize_reference_side_dynamics_pairs(&pairs)
}

fn summarize_side_dynamics_window_consistency(
    windows: &[SideDynamicsWindow],
    max_lag_bins: usize,
) -> Option<ReferenceSideDynamicsConsistency> {
    if windows.len() < 2 {
        return None;
    }

    let mut pairs = Vec::new();
    for left_idx in 0..windows.len() {
        for right_idx in left_idx + 1..windows.len() {
            let left = &windows[left_idx];
            let right = &windows[right_idx];
            let metrics =
                analyze_transient_alignment(&left.envelope, &right.envelope, max_lag_bins)?;
            pairs.push(ReferenceSideDynamicsPairAnalysis {
                left_name: format!("ref_{left_idx}"),
                right_name: format!("ref_{right_idx}"),
                weight: (left.weight + right.weight) * 0.5,
                left_ratio: left.ratio,
                right_ratio: right.ratio,
                metrics,
            });
        }
    }

    summarize_reference_side_dynamics_pairs(&pairs)
}

fn reference_side_transient_consistency_weight(
    consistency: ReferenceSideDynamicsConsistency,
) -> f32 {
    let stability = (consistency.average_adjusted_envelope_corr.clamp(0.0, 1.0) * 0.20
        + consistency.average_adjusted_transient_corr.clamp(0.0, 1.0) * 0.35
        + consistency.worst_adjusted_transient_corr.clamp(0.0, 1.0) * 0.25
        + consistency.average_transient_rms_fit.clamp(0.0, 1.0) * 0.20)
        .clamp(0.0, 1.0);

    0.20 + 0.80 * stability * stability
}

fn sectioned_side_transient_reliability(sections: &[FixedSideConsensusSectionTarget]) -> f32 {
    if sections.is_empty() {
        return 1.0;
    }

    let uniform = vec![1.0; sections.len()];
    weighted_average(
        &sections
            .iter()
            .map(|section| section.transient_reliability)
            .collect::<Vec<_>>(),
        &uniform,
    )
}

fn dominant_section_impact_index(
    section_finals: &[f32],
    section_weights: &[f32],
) -> Option<(usize, f32)> {
    section_impact_ranking(section_finals, section_weights)
        .into_iter()
        .next()
}

fn section_impact_ranking(section_finals: &[f32], section_weights: &[f32]) -> Vec<(usize, f32)> {
    if section_finals.is_empty()
        || section_finals.len() != section_weights.len()
        || !section_weights.iter().any(|&weight| weight > 0.0)
    {
        return Vec::new();
    }

    let weight_sum: f32 = section_weights.iter().copied().sum();
    if weight_sum <= 1e-9 {
        return Vec::new();
    }

    let mut ranked = section_finals
        .iter()
        .copied()
        .zip(section_weights.iter().copied())
        .enumerate()
        .map(|(idx, (final_score, weight))| {
            let normalized_weight = weight / weight_sum;
            let deficit = (1.0 - final_score.clamp(0.0, 1.0)).max(0.0);
            (idx, normalized_weight * deficit)
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked
}

fn envelope_transient_profile(envelope: &[f32]) -> Vec<f32> {
    envelope
        .windows(2)
        .map(|window| window[1] - window[0])
        .collect()
}

fn score_hybrid_mono_consensus_candidate(
    name: &str,
    mono_score: ConsensusCandidateScore,
    side_scores: &[f32],
    side_weights: &[f32],
    side_reliability: f32,
) -> ConsensusCandidateScore {
    if side_scores.is_empty() || side_scores.len() != side_weights.len() {
        return ConsensusCandidateScore {
            name: name.to_owned(),
            average_score: mono_score.average_score,
            disagreement_penalty: mono_score.disagreement_penalty,
            final_score: mono_score.final_score,
        };
    }

    let side_average = weighted_average(side_scores, side_weights);
    let side_penalty = weighted_mean_absolute_deviation(side_scores, side_weights);
    let side_reliability = side_reliability.clamp(0.0, 1.0);
    let side_mix = 0.10 * side_reliability;
    let average_score = mono_score.average_score * (1.0 - side_mix) + side_average * side_mix;
    let disagreement_penalty =
        mono_score.disagreement_penalty + side_penalty * 0.05 * side_reliability;

    ConsensusCandidateScore {
        name: name.to_owned(),
        average_score,
        disagreement_penalty,
        final_score: average_score - disagreement_penalty,
    }
}

fn mean_abs_delta(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    a.iter()
        .zip(b)
        .take(n)
        .map(|(&av, &bv)| (av - bv).abs())
        .sum::<f32>()
        / n as f32
}

fn largest_band_deltas_hz(
    a: &[f32],
    b: &[f32],
    frame_len: usize,
    sample_rate: u32,
    bins: &[usize],
    top_n: usize,
) -> Vec<(f32, f32)> {
    let n = a.len().min(b.len()).min(bins.len());
    let mut deltas: Vec<_> = (0..n)
        .map(|idx| {
            let delta_db = (b[idx] - a[idx]) * (10.0 / std::f32::consts::LN_10);
            (bin_center_hz(frame_len, sample_rate, bins[idx]), delta_db)
        })
        .collect();
    deltas.sort_by(|a, b| {
        b.1.abs()
            .partial_cmp(&a.1.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    deltas.truncate(top_n.min(deltas.len()));
    deltas
}

fn segmented_residual_profiles(
    emu: &[f32],
    reference: &[f32],
    segment_len: usize,
    frame_len: usize,
    hop_len: usize,
    bins: &[usize],
) -> Vec<Vec<f32>> {
    let len = emu.len().min(reference.len());
    if len < frame_len || segment_len < frame_len {
        return Vec::new();
    }

    let mut profiles = Vec::new();
    let mut start = 0usize;
    while start + segment_len <= len {
        let emu_spec =
            average_log_spectrum(&emu[start..start + segment_len], frame_len, hop_len, bins);
        let ref_spec = average_log_spectrum(
            &reference[start..start + segment_len],
            frame_len,
            hop_len,
            bins,
        );
        let residual = ref_spec
            .iter()
            .zip(&emu_spec)
            .map(|(&ref_bin, &emu_bin)| ref_bin - emu_bin)
            .collect();
        profiles.push(residual);
        start += segment_len;
    }

    profiles
}

fn residual_profile_stability(profiles: &[Vec<f32>]) -> f32 {
    if profiles.is_empty() {
        return 0.0;
    }

    let weights = vec![1.0; profiles.len()];
    let mean_profile = weighted_average_vectors(profiles, &weights);
    profiles
        .iter()
        .map(|profile| mean_abs_delta(profile, &mean_profile))
        .sum::<f32>()
        / profiles.len() as f32
}

fn partition_window_sections(total_len: usize, section_len: usize) -> Vec<(usize, usize)> {
    if total_len == 0 || section_len == 0 {
        return Vec::new();
    }

    let mut sections = Vec::new();
    let mut start = 0usize;
    while start < total_len {
        let len = section_len.min(total_len - start);
        sections.push((start, len));
        start += section_len;
    }

    sections
}

fn best_reference_section_offset(
    emu: &[f32],
    reference: &[f32],
    emu_start: usize,
    ref_start: usize,
    len: usize,
    step: usize,
    max_steps: usize,
) -> Option<(isize, f32)> {
    if len == 0 || step == 0 || emu_start.checked_add(len)? > emu.len() {
        return None;
    }

    let emu_env = rms_envelope(&emu[emu_start..emu_start + len], step);
    if emu_env.len() < 2 {
        return None;
    }

    let mut offsets = Vec::with_capacity(max_steps * 2 + 1);
    offsets.push(0isize);
    for delta in 1..=max_steps as isize {
        offsets.push(delta);
        offsets.push(-delta);
    }

    let mut best: Option<(isize, f32)> = None;
    for offset_steps in offsets {
        let sample_offset = offset_steps * step as isize;
        let candidate_start = if sample_offset >= 0 {
            match ref_start.checked_add(sample_offset as usize) {
                Some(start) => start,
                None => continue,
            }
        } else {
            match ref_start.checked_sub((-sample_offset) as usize) {
                Some(start) => start,
                None => continue,
            }
        };
        let Some(candidate_end) = candidate_start.checked_add(len) else {
            continue;
        };
        if candidate_end > reference.len() {
            continue;
        }

        let ref_env = rms_envelope(&reference[candidate_start..candidate_end], step);
        if ref_env.len() != emu_env.len() {
            continue;
        }

        let corr = cross_correlation(&emu_env, &ref_env);
        if best.is_none_or(|(_, best_corr)| corr > best_corr) {
            best = Some((sample_offset, corr));
        }
    }

    best
}

fn score_aligned_section(
    emu_samples: &[f32],
    ref_samples: &[f32],
    emu_start: usize,
    ref_start: usize,
    len: usize,
) -> Option<FixedTrustedWindowMetrics> {
    if len == 0 {
        return None;
    }

    let emu_left: Vec<f32> = emu_samples.iter().step_by(2).copied().collect();
    let ref_left: Vec<f32> = ref_samples.iter().step_by(2).copied().collect();
    if emu_start.checked_add(len)? > emu_left.len() || ref_start.checked_add(len)? > ref_left.len()
    {
        return None;
    }

    let emu_stereo = stereo_window(emu_samples, emu_start, len);
    let ref_stereo = stereo_window(ref_samples, ref_start, len);
    let (emu_left_window, _) = stereo_left_right(emu_stereo);
    let (ref_left_window, _) = stereo_left_right(ref_stereo);
    let (emu_mid, emu_side) = stereo_mid_side(emu_stereo);
    let (ref_mid, ref_side) = stereo_mid_side(ref_stereo);
    let spectral_bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 12_000.0, 24);

    Some(FixedTrustedWindowMetrics {
        raw: cross_correlation(&emu_left_window, &ref_left_window),
        spectral: spectral_similarity(
            &emu_left_window,
            &ref_left_window,
            2048,
            1024,
            &spectral_bins,
        ),
        rms_ratio: rms(&emu_left_window) / rms(&ref_left_window).max(1e-9),
        mid: spectral_similarity(&emu_mid, &ref_mid, 2048, 1024, &spectral_bins),
        side: spectral_similarity(&emu_side, &ref_side, 2048, 1024, &spectral_bins),
    })
}

fn fixed_window_fit_score(metrics: FixedTrustedWindowMetrics) -> f32 {
    (metrics.spectral + metrics.mid + metrics.side + rms_fit_score(metrics.rms_ratio)) / 4.0
}

fn normalized_section_score(
    metrics: FixedTrustedWindowMetrics,
    self_window: ReferenceConsistencyWindow,
) -> f32 {
    let spectral_score = normalized_score(metrics.spectral, self_window.left_spectral);
    let mid_score = normalized_score(metrics.mid, self_window.mid_spectral);
    let side_score = normalized_score(metrics.side, self_window.side_spectral);
    let rms_score = normalized_score(
        rms_fit_score(metrics.rms_ratio),
        rms_fit_score(self_window.rms_ratio),
    );

    (spectral_score + mid_score + side_score + rms_score) / 4.0
}

fn normalized_section_mono_score(
    metrics: FixedTrustedWindowMetrics,
    self_window: ReferenceConsistencyWindow,
) -> f32 {
    let spectral_score = normalized_score(metrics.spectral, self_window.left_spectral);
    let mid_score = normalized_score(metrics.mid, self_window.mid_spectral);
    let rms_score = normalized_score(
        rms_fit_score(metrics.rms_ratio),
        rms_fit_score(self_window.rms_ratio),
    );

    spectral_score * 0.40 + mid_score * 0.40 + rms_score * 0.20
}

fn normalized_section_side_score(
    metrics: FixedTrustedWindowMetrics,
    self_window: ReferenceConsistencyWindow,
) -> f32 {
    normalized_score(metrics.side, self_window.side_spectral)
}

fn section_self_consistency_weight(self_window: ReferenceConsistencyWindow) -> f32 {
    let rms_fit = rms_fit_score(self_window.rms_ratio);
    let stability = (self_window.left_spectral * 0.30
        + self_window.mid_spectral * 0.30
        + self_window.side_spectral * 0.20
        + rms_fit * 0.20)
        .clamp(0.0, 1.0);

    0.20 + 0.80 * stability * stability
}

fn mono_first_section_self_consistency_weight(self_window: ReferenceConsistencyWindow) -> f32 {
    let rms_fit = rms_fit_score(self_window.rms_ratio);
    let stability = (self_window.left_spectral * 0.35
        + self_window.mid_spectral * 0.35
        + self_window.side_spectral * 0.10
        + rms_fit * 0.20)
        .clamp(0.0, 1.0);

    0.20 + 0.80 * stability * stability
}

fn side_section_consistency_weight(self_window: ReferenceConsistencyWindow) -> f32 {
    0.20 + 0.80 * self_window.side_spectral.clamp(0.0, 1.0).powi(2)
}

fn mono_consensus_reference_weight(selection: TrustedWindowSelection) -> f32 {
    let rms_fit = rms_fit_score(selection.trusted_self_rms);
    let stability = (selection.trusted_self_left * 0.40
        + selection.trusted_self_mid * 0.40
        + selection.trusted_self_side * 0.10
        + rms_fit * 0.10)
        .clamp(0.0, 1.0);

    0.20 + 0.80 * stability * stability
}

fn repeated_consecutive_ratio(samples: &[f32]) -> f32 {
    if samples.len() < 2 {
        return 0.0;
    }

    let repeated = samples.windows(2).filter(|w| w[0] == w[1]).count();
    repeated as f32 / (samples.len() - 1) as f32
}

fn count_near_clipped(samples: &[f32], threshold: f32) -> usize {
    samples
        .iter()
        .filter(|&&sample| sample.abs() >= threshold)
        .count()
}

fn sample_jump_metrics(samples: &[f32], jump_threshold: f32) -> (f32, f32, usize) {
    if samples.len() < 2 {
        return (0.0, 0.0, 0);
    }

    let deltas: Vec<f32> = samples.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
    let mean_delta = deltas.iter().sum::<f32>() / deltas.len() as f32;
    let max_delta = deltas.iter().copied().fold(0.0f32, f32::max);
    let jump_count = deltas
        .iter()
        .filter(|&&delta| delta >= jump_threshold)
        .count();
    (mean_delta, max_delta, jump_count)
}

fn bin_center_hz(frame_len: usize, sample_rate: u32, bin: usize) -> f32 {
    bin as f32 * sample_rate as f32 / frame_len as f32
}

fn spectral_similarity(
    a: &[f32],
    b: &[f32],
    frame_len: usize,
    hop_len: usize,
    bins: &[usize],
) -> f32 {
    let spec_a = average_log_spectrum(a, frame_len, hop_len, bins);
    let spec_b = average_log_spectrum(b, frame_len, hop_len, bins);
    cosine_similarity(&spec_a, &spec_b)
}

fn solve_linear_system_5x5(mut a: [[f64; 5]; 5], mut b: [f64; 5]) -> Option<[f32; 5]> {
    for pivot in 0..5 {
        let mut best_row = pivot;
        for row in (pivot + 1)..5 {
            if a[row][pivot].abs() > a[best_row][pivot].abs() {
                best_row = row;
            }
        }
        if a[best_row][pivot].abs() < 1e-12 {
            return None;
        }
        if best_row != pivot {
            a.swap(pivot, best_row);
            b.swap(pivot, best_row);
        }

        let diag = a[pivot][pivot];
        for col in pivot..5 {
            a[pivot][col] /= diag;
        }
        b[pivot] /= diag;

        for row in 0..5 {
            if row == pivot {
                continue;
            }
            let factor = a[row][pivot];
            if factor.abs() < 1e-18 {
                continue;
            }
            for col in pivot..5 {
                a[row][col] -= factor * a[pivot][col];
            }
            b[row] -= factor * b[pivot];
        }
    }

    Some(b.map(|value| value as f32))
}

fn fit_two_source_mix(source_a: &[f32], source_b: &[f32], target: &[f32]) -> Option<(f32, f32)> {
    let n = source_a.len().min(source_b.len()).min(target.len());
    if n == 0 {
        return None;
    }

    let mut saa = 0.0f64;
    let mut sbb = 0.0f64;
    let mut sab = 0.0f64;
    let mut sat = 0.0f64;
    let mut sbt = 0.0f64;

    for idx in 0..n {
        let a = f64::from(source_a[idx]);
        let b = f64::from(source_b[idx]);
        let t = f64::from(target[idx]);
        saa += a * a;
        sbb += b * b;
        sab += a * b;
        sat += a * t;
        sbt += b * t;
    }

    let det = saa * sbb - sab * sab;
    if det.abs() < 1e-12 {
        return None;
    }

    let a_scale = (sat * sbb - sbt * sab) / det;
    let b_scale = (sbt * saa - sat * sab) / det;
    Some((a_scale as f32, b_scale as f32))
}

fn mix_two_sources(source_a: &[f32], source_b: &[f32], a_scale: f32, b_scale: f32) -> Vec<f32> {
    source_a
        .iter()
        .zip(source_b.iter())
        .map(|(&a, &b)| a * a_scale + b * b_scale)
        .collect()
}

fn fit_fir_taps_least_squares(input: &[f32], target: &[f32], ridge: f32) -> [f32; 5] {
    let n = input.len().min(target.len());
    if n < 64 {
        return [1.0, 0.0, 0.0, 0.0, 0.0];
    }

    let mut ata = [[0.0f64; 5]; 5];
    let mut atb = [0.0f64; 5];
    for idx in 4..n {
        let x = [
            f64::from(input[idx]),
            f64::from(input[idx - 1]),
            f64::from(input[idx - 2]),
            f64::from(input[idx - 3]),
            f64::from(input[idx - 4]),
        ];
        let y = f64::from(target[idx]);
        for row in 0..5 {
            atb[row] += x[row] * y;
            for col in 0..5 {
                ata[row][col] += x[row] * x[col];
            }
        }
    }
    for (idx, row) in ata.iter_mut().enumerate() {
        row[idx] += f64::from(ridge);
    }

    solve_linear_system_5x5(ata, atb).unwrap_or([1.0, 0.0, 0.0, 0.0, 0.0])
}

fn mix_fir_taps(a: [f32; 5], b: [f32; 5], t: f32) -> [f32; 5] {
    let mut mixed = [0.0; 5];
    for idx in 0..5 {
        mixed[idx] = a[idx] * (1.0 - t) + b[idx] * t;
    }
    mixed
}

#[derive(Debug, Clone)]
struct GhzTimedTrace {
    extra_frames: u32,
    capture_start_tick: u64,
    capture_start_sample: u64,
    end_tick: u64,
    ym_writes: Vec<TimedYm2612Write>,
    psg_writes: Vec<TimedPsgWrite>,
}

#[test]
fn best_windowed_match_handles_loop_shifted_reference() {
    let intro = [0.15, 0.35, 0.05, 0.25, 0.18, 0.32, 0.08, 0.22];
    let loop_phrase = [0.0, 0.8, 0.2, 0.9, 0.1, 0.7, 0.3, 1.0];

    let mut emu = intro.to_vec();
    for _ in 0..3 {
        emu.extend_from_slice(&loop_phrase);
    }

    let mut reference = Vec::new();
    for _ in 0..4 {
        reference.extend_from_slice(&loop_phrase);
    }

    let (corr, lag) = best_lagged_correlation(&emu, &reference, reference.len());
    assert!(
        corr < 0.95,
        "whole-track correlation should still be diluted by the non-loop intro"
    );

    let best = best_windowed_match_at_lag(&emu, &reference, lag, loop_phrase.len())
        .expect("expected a local loop-aligned match");

    assert!(
        best.corr > 0.999,
        "expected near-perfect local loop match, got {:.4}",
        best.corr
    );
    assert_eq!(
        &emu[best.a_start..best.a_start + best.len],
        &reference[best.b_start..best.b_start + best.len]
    );
}

#[test]
fn spectral_similarity_is_phase_tolerant() {
    let sr = SAMPLE_RATE as f32;
    let samples: Vec<f32> = (0..SAMPLE_RATE as usize)
        .map(|i| ((2.0 * std::f32::consts::PI * 440.0 * i as f32) / sr).sin())
        .collect();
    let phase_shifted: Vec<f32> = (0..SAMPLE_RATE as usize)
        .map(|i| {
            ((2.0 * std::f32::consts::PI * 440.0 * i as f32) / sr + std::f32::consts::FRAC_PI_2)
                .sin()
        })
        .collect();
    let bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 5_000.0, 16);

    let raw_corr = cross_correlation(&samples, &phase_shifted);
    let spectral = spectral_similarity(&samples, &phase_shifted, 2048, 1024, &bins);

    assert!(
        raw_corr.abs() < 0.1,
        "expected raw correlation to care about phase"
    );
    assert!(
        spectral > 0.98,
        "expected spectral similarity to ignore phase, got {spectral:.4}"
    );
}

#[test]
fn spectral_similarity_penalizes_timbre_changes() {
    let sr = SAMPLE_RATE as f32;
    let sine: Vec<f32> = (0..SAMPLE_RATE as usize)
        .map(|i| ((2.0 * std::f32::consts::PI * 440.0 * i as f32) / sr).sin())
        .collect();
    let mut state = 0x1234_5678u32;
    let noise: Vec<f32> = (0..SAMPLE_RATE as usize)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (((state >> 8) & 0xFFFF) as f32 / 32767.5) - 1.0
        })
        .collect();
    let bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 8_000.0, 20);

    let spectral = spectral_similarity(&sine, &noise, 2048, 1024, &bins);
    assert!(
        spectral < 0.7,
        "expected timbre mismatch to lower spectral similarity, got {spectral:.4}"
    );
}

#[test]
fn spectral_similarity_is_gain_tolerant() {
    let sr = SAMPLE_RATE as f32;
    let samples: Vec<f32> = (0..SAMPLE_RATE as usize)
        .map(|i| ((2.0 * std::f32::consts::PI * 440.0 * i as f32) / sr).sin())
        .collect();
    let louder: Vec<f32> = samples.iter().map(|&sample| sample * 2.5).collect();
    let bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 8_000.0, 20);

    let spectral = spectral_similarity(&samples, &louder, 2048, 1024, &bins);
    assert!(
        spectral > 0.98,
        "expected spectral similarity to ignore overall gain, got {spectral:.4}"
    );
}

#[test]
fn average_phase_delta_reports_known_phase_offset() {
    let sr = SAMPLE_RATE as f32;
    let samples: Vec<f32> = (0..SAMPLE_RATE as usize)
        .map(|i| ((2.0 * std::f32::consts::PI * 440.0 * i as f32) / sr).sin())
        .collect();
    let shifted: Vec<f32> = (0..SAMPLE_RATE as usize)
        .map(|i| {
            ((2.0 * std::f32::consts::PI * 440.0 * i as f32) / sr + std::f32::consts::FRAC_PI_2)
                .sin()
        })
        .collect();
    let bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 2_000.0, 8);
    let phase = average_phase_delta(&samples, &shifted, 2048, 1024, &bins);
    let loudest_bin = bins
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            let da = (bin_center_hz(2048, SAMPLE_RATE, **a) - 440.0).abs();
            let db = (bin_center_hz(2048, SAMPLE_RATE, **b) - 440.0).abs();
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(idx, _)| idx)
        .unwrap();
    let (phase_deg, coherence) = phase[loudest_bin];

    assert!(
        coherence > 0.95,
        "expected strong coherence for a fixed phase shift, got {coherence:.4}"
    );
    assert!(
        (phase_deg - 90.0).abs() < 15.0 || (phase_deg + 270.0).abs() < 15.0,
        "expected about +90 degrees of phase delta, got {phase_deg:.2}"
    );
}

#[test]
fn fit_two_source_mix_recovers_known_coefficients() {
    let source_a = [1.0, 0.5, -0.25, 0.75, -0.5, 0.25];
    let source_b = [0.0, 1.0, 0.5, -0.25, 0.75, -1.0];
    let target: Vec<f32> = source_a
        .iter()
        .zip(source_b.iter())
        .map(|(&a, &b)| a * 1.25 + b * 0.6)
        .collect();

    let (a_scale, b_scale) =
        fit_two_source_mix(&source_a, &source_b, &target).expect("expected solvable fit");

    assert!(
        (a_scale - 1.25).abs() < 1e-4,
        "expected source A scale near 1.25, got {a_scale:.6}"
    );
    assert!(
        (b_scale - 0.6).abs() < 1e-4,
        "expected source B scale near 0.6, got {b_scale:.6}"
    );
}

#[test]
fn fit_two_source_mix_rejects_degenerate_sources() {
    let source = [0.5, -0.25, 0.75, -0.5];
    let target = [0.1, 0.2, 0.3, 0.4];

    assert!(
        fit_two_source_mix(&source, &source, &target).is_none(),
        "expected identical sources to be unsolvable"
    );
}

#[test]
fn mean_phase_coherence_reports_perfect_alignment_for_identical_signals() {
    let sr = SAMPLE_RATE as f32;
    let samples: Vec<f32> = (0..SAMPLE_RATE as usize)
        .map(|i| ((2.0 * std::f32::consts::PI * 440.0 * i as f32) / sr).sin())
        .collect();
    let bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 2_000.0, 8);
    let phase = average_phase_delta(&samples, &samples, 2048, 1024, &bins);
    let coherence = mean_phase_coherence(&phase);

    assert!(
        coherence > 0.99,
        "expected near-perfect coherence for identical signals, got {coherence:.4}"
    );
}

#[test]
fn best_reference_self_window_at_offset_prefers_stable_region() {
    let stable: Vec<f32> = (0..16)
        .flat_map(|i| {
            let t = i as f32 / 16.0;
            let left = (2.0 * std::f32::consts::PI * t).sin();
            let right = (2.0 * std::f32::consts::PI * t + 0.2).sin();
            [left, right]
        })
        .collect();
    let unstable_a: Vec<f32> = (0..16)
        .flat_map(|i| {
            let t = i as f32 / 16.0;
            let left = (6.0 * std::f32::consts::PI * t).sin() * 0.5;
            let right = (8.0 * std::f32::consts::PI * t).cos() * 0.5;
            [left, right]
        })
        .collect();
    let unstable_b: Vec<f32> = (0..16)
        .flat_map(|i| {
            let t = i as f32 / 16.0;
            let left = (10.0 * std::f32::consts::PI * t).sin() * 0.2;
            let right = (12.0 * std::f32::consts::PI * t).cos() * 0.8;
            [left, right]
        })
        .collect();

    let mut reference = Vec::new();
    reference.extend_from_slice(&stable);
    reference.extend_from_slice(&unstable_a);
    reference.extend_from_slice(&stable);
    reference.extend_from_slice(&unstable_b);

    let bins = log_frequency_bins(16, SAMPLE_RATE, 80.0, 5_000.0, 6);
    let best = best_reference_self_window_at_offset(&reference, 32, 16, 16, 8, &bins)
        .expect("expected a stable repeated window");

    assert_eq!(best.prior_start, 0);
    assert_eq!(best.current_start, 32);
    assert!(
        best.left_spectral > 0.99 && best.mid_spectral > 0.99,
        "expected the stable repeated region to be nearly identical, got left {:.4} mid {:.4}",
        best.left_spectral,
        best.mid_spectral
    );
}

#[test]
fn consensus_candidate_prefers_higher_average_quality() {
    let better = [
        TrustedReferenceMetrics {
            env_corr: 0.5,
            local_env_corr: 0.72,
            trusted_score: 0.9,
            trusted_self_left: 0.9,
            trusted_self_mid: 0.9,
            trusted_self_side: 0.8,
            trusted_self_rms: 1.0,
            trusted_emu_raw: 0.02,
            trusted_emu_spectral: 0.90,
            trusted_emu_rms: 0.98,
            trusted_emu_mid: 0.91,
            trusted_emu_side: 0.40,
        },
        TrustedReferenceMetrics {
            env_corr: 0.4,
            local_env_corr: 0.70,
            trusted_score: 0.9,
            trusted_self_left: 0.9,
            trusted_self_mid: 0.9,
            trusted_self_side: 0.7,
            trusted_self_rms: 1.0,
            trusted_emu_raw: 0.01,
            trusted_emu_spectral: 0.88,
            trusted_emu_rms: 1.02,
            trusted_emu_mid: 0.89,
            trusted_emu_side: 0.35,
        },
    ];
    let worse = [
        TrustedReferenceMetrics {
            trusted_emu_spectral: 0.84,
            trusted_emu_rms: 1.05,
            trusted_emu_mid: 0.85,
            trusted_emu_side: 0.30,
            ..better[0]
        },
        TrustedReferenceMetrics {
            trusted_emu_spectral: 0.82,
            trusted_emu_rms: 1.07,
            trusted_emu_mid: 0.83,
            trusted_emu_side: 0.28,
            ..better[1]
        },
    ];

    let better_score = score_consensus_candidate("better", &better);
    let worse_score = score_consensus_candidate("worse", &worse);
    assert!(
        better_score.final_score > worse_score.final_score,
        "expected better average metrics to win: {:.4} vs {:.4}",
        better_score.final_score,
        worse_score.final_score
    );
}

#[test]
fn consensus_candidate_penalizes_cross_reference_disagreement() {
    let stable = [
        TrustedReferenceMetrics {
            env_corr: 0.5,
            local_env_corr: 0.70,
            trusted_score: 0.9,
            trusted_self_left: 0.9,
            trusted_self_mid: 0.9,
            trusted_self_side: 0.8,
            trusted_self_rms: 1.0,
            trusted_emu_raw: 0.02,
            trusted_emu_spectral: 0.88,
            trusted_emu_rms: 1.00,
            trusted_emu_mid: 0.89,
            trusted_emu_side: 0.35,
        },
        TrustedReferenceMetrics {
            env_corr: 0.4,
            local_env_corr: 0.70,
            trusted_score: 0.9,
            trusted_self_left: 0.9,
            trusted_self_mid: 0.9,
            trusted_self_side: 0.7,
            trusted_self_rms: 1.0,
            trusted_emu_raw: 0.01,
            trusted_emu_spectral: 0.88,
            trusted_emu_rms: 1.00,
            trusted_emu_mid: 0.89,
            trusted_emu_side: 0.35,
        },
    ];
    let unstable = [
        TrustedReferenceMetrics {
            trusted_emu_spectral: 0.95,
            trusted_emu_rms: 0.90,
            trusted_emu_mid: 0.95,
            trusted_emu_side: 0.50,
            ..stable[0]
        },
        TrustedReferenceMetrics {
            trusted_emu_spectral: 0.81,
            trusted_emu_rms: 1.14,
            trusted_emu_mid: 0.83,
            trusted_emu_side: 0.20,
            ..stable[1]
        },
    ];

    let stable_score = score_consensus_candidate("stable", &stable);
    let unstable_score = score_consensus_candidate("unstable", &unstable);
    assert!(
        stable_score.disagreement_penalty < unstable_score.disagreement_penalty,
        "expected disagreement penalty to increase: {:.4} vs {:.4}",
        stable_score.disagreement_penalty,
        unstable_score.disagreement_penalty
    );
    assert!(
        stable_score.final_score > unstable_score.final_score,
        "expected stable candidate to win after penalty: {:.4} vs {:.4}",
        stable_score.final_score,
        unstable_score.final_score
    );
}

#[test]
fn normalized_reference_score_respects_reference_self_ceiling() {
    let stronger_reference = TrustedReferenceMetrics {
        env_corr: 0.5,
        local_env_corr: 0.72,
        trusted_score: 0.90,
        trusted_self_left: 0.90,
        trusted_self_mid: 0.90,
        trusted_self_side: 0.80,
        trusted_self_rms: 1.00,
        trusted_emu_raw: 0.02,
        trusted_emu_spectral: 0.81,
        trusted_emu_rms: 0.95,
        trusted_emu_mid: 0.81,
        trusted_emu_side: 0.40,
    };
    let weaker_reference = TrustedReferenceMetrics {
        env_corr: 0.4,
        local_env_corr: 0.60,
        trusted_score: 0.75,
        trusted_self_left: 0.75,
        trusted_self_mid: 0.75,
        trusted_self_side: 0.50,
        trusted_self_rms: 0.95,
        trusted_emu_raw: 0.01,
        trusted_emu_spectral: 0.675,
        trusted_emu_rms: 0.9025,
        trusted_emu_mid: 0.675,
        trusted_emu_side: 0.25,
    };

    let stronger_score = normalized_reference_score(stronger_reference);
    let weaker_score = normalized_reference_score(weaker_reference);
    assert!(
        (stronger_score - weaker_score).abs() < 0.02,
        "expected normalization to treat equal relative fit similarly: {:.4} vs {:.4}",
        stronger_score,
        weaker_score
    );
}

#[test]
fn weighted_consensus_prefers_better_result_on_more_reliable_reference() {
    let reliable_reference = TrustedReferenceMetrics {
        env_corr: 0.5,
        local_env_corr: 0.74,
        trusted_score: 0.92,
        trusted_self_left: 0.92,
        trusted_self_mid: 0.93,
        trusted_self_side: 0.82,
        trusted_self_rms: 0.99,
        trusted_emu_raw: 0.02,
        trusted_emu_spectral: 0.90,
        trusted_emu_rms: 0.99,
        trusted_emu_mid: 0.91,
        trusted_emu_side: 0.44,
    };
    let noisy_reference = TrustedReferenceMetrics {
        env_corr: 0.3,
        local_env_corr: 0.46,
        trusted_score: 0.76,
        trusted_self_left: 0.78,
        trusted_self_mid: 0.77,
        trusted_self_side: 0.42,
        trusted_self_rms: 0.86,
        trusted_emu_raw: 0.01,
        trusted_emu_spectral: 0.70,
        trusted_emu_rms: 1.08,
        trusted_emu_mid: 0.71,
        trusted_emu_side: 0.18,
    };

    let candidate_a = [reliable_reference, noisy_reference];
    let candidate_b = [
        TrustedReferenceMetrics {
            trusted_emu_spectral: 0.86,
            trusted_emu_rms: 1.03,
            trusted_emu_mid: 0.87,
            trusted_emu_side: 0.36,
            local_env_corr: 0.69,
            ..reliable_reference
        },
        TrustedReferenceMetrics {
            trusted_emu_spectral: 0.75,
            trusted_emu_rms: 1.00,
            trusted_emu_mid: 0.76,
            trusted_emu_side: 0.26,
            local_env_corr: 0.56,
            ..noisy_reference
        },
    ];

    let score_a = score_consensus_candidate("candidate_a", &candidate_a);
    let score_b = score_consensus_candidate("candidate_b", &candidate_b);
    assert!(
        score_a.final_score > score_b.final_score,
        "expected the candidate that wins on the more reliable reference to survive weighting: {:.4} vs {:.4}",
        score_a.final_score,
        score_b.final_score
    );
}

#[test]
fn normalized_section_score_respects_section_self_ceiling() {
    let stronger_self = ReferenceConsistencyWindow {
        score: 0.82,
        prior_start: 0,
        current_start: 8,
        len: 8,
        left_spectral: 0.90,
        mid_spectral: 0.88,
        side_spectral: 0.70,
        rms_ratio: 1.00,
    };
    let weaker_self = ReferenceConsistencyWindow {
        score: 0.74,
        prior_start: 0,
        current_start: 8,
        len: 8,
        left_spectral: 0.72,
        mid_spectral: 0.704,
        side_spectral: 0.42,
        rms_ratio: 0.92,
    };
    let stronger_metrics = FixedTrustedWindowMetrics {
        raw: 0.0,
        spectral: 0.81,
        rms_ratio: 0.95,
        mid: 0.792,
        side: 0.35,
    };
    let weaker_metrics = FixedTrustedWindowMetrics {
        raw: 0.0,
        spectral: 0.648,
        rms_ratio: 0.874,
        mid: 0.6336,
        side: 0.21,
    };

    let stronger_score = normalized_section_score(stronger_metrics, stronger_self);
    let weaker_score = normalized_section_score(weaker_metrics, weaker_self);
    assert!(
        (stronger_score - weaker_score).abs() < 0.02,
        "expected section normalization to treat equal relative fit similarly: {:.4} vs {:.4}",
        stronger_score,
        weaker_score
    );
}

#[test]
fn section_self_consistency_weight_penalizes_weak_side_and_rms() {
    let stable = ReferenceConsistencyWindow {
        score: 0.84,
        prior_start: 0,
        current_start: 8,
        len: 8,
        left_spectral: 0.86,
        mid_spectral: 0.84,
        side_spectral: 0.80,
        rms_ratio: 0.98,
    };
    let unstable = ReferenceConsistencyWindow {
        side_spectral: 0.20,
        rms_ratio: 0.76,
        ..stable
    };

    let stable_weight = section_self_consistency_weight(stable);
    let unstable_weight = section_self_consistency_weight(unstable);
    assert!(
        stable_weight > unstable_weight,
        "expected unstable section to get less weight: {:.4} vs {:.4}",
        stable_weight,
        unstable_weight
    );
}

#[test]
fn sectioned_consensus_prefers_better_result_on_more_section_reliable_reference() {
    let reliable_reference = TrustedReferenceMetrics {
        env_corr: 0.5,
        local_env_corr: 0.74,
        trusted_score: 0.92,
        trusted_self_left: 0.92,
        trusted_self_mid: 0.93,
        trusted_self_side: 0.82,
        trusted_self_rms: 0.99,
        trusted_emu_raw: 0.02,
        trusted_emu_spectral: 0.90,
        trusted_emu_rms: 0.99,
        trusted_emu_mid: 0.91,
        trusted_emu_side: 0.44,
    };
    let noisy_reference = TrustedReferenceMetrics {
        env_corr: 0.3,
        local_env_corr: 0.46,
        trusted_score: 0.76,
        trusted_self_left: 0.78,
        trusted_self_mid: 0.77,
        trusted_self_side: 0.42,
        trusted_self_rms: 0.86,
        trusted_emu_raw: 0.01,
        trusted_emu_spectral: 0.70,
        trusted_emu_rms: 1.08,
        trusted_emu_mid: 0.71,
        trusted_emu_side: 0.18,
    };
    let candidate_a_sections = [
        SectionedReferenceScore {
            normalized_score: 0.90,
            section_reliability: 0.88,
            worst_section_score: 0.72,
        },
        SectionedReferenceScore {
            normalized_score: 0.73,
            section_reliability: 0.36,
            worst_section_score: 0.18,
        },
    ];
    let candidate_b_sections = [
        SectionedReferenceScore {
            normalized_score: 0.84,
            section_reliability: 0.88,
            worst_section_score: 0.64,
        },
        SectionedReferenceScore {
            normalized_score: 0.88,
            section_reliability: 0.36,
            worst_section_score: 0.28,
        },
    ];

    let score_a = score_sectioned_consensus_candidate(
        "candidate_a",
        &[reliable_reference, noisy_reference],
        &candidate_a_sections,
    );
    let score_b = score_sectioned_consensus_candidate(
        "candidate_b",
        &[reliable_reference, noisy_reference],
        &candidate_b_sections,
    );
    assert!(
        score_a.final_score > score_b.final_score,
        "expected sectioned weighting to favor the candidate that wins on the more reliable reference: {:.4} vs {:.4}",
        score_a.final_score,
        score_b.final_score
    );
}

#[test]
fn normalized_section_mono_score_respects_section_self_ceiling() {
    let stronger_self = ReferenceConsistencyWindow {
        score: 0.82,
        prior_start: 0,
        current_start: 8,
        len: 8,
        left_spectral: 0.90,
        mid_spectral: 0.88,
        side_spectral: 0.70,
        rms_ratio: 1.00,
    };
    let weaker_self = ReferenceConsistencyWindow {
        score: 0.74,
        prior_start: 0,
        current_start: 8,
        len: 8,
        left_spectral: 0.72,
        mid_spectral: 0.704,
        side_spectral: 0.42,
        rms_ratio: 0.92,
    };
    let stronger_metrics = FixedTrustedWindowMetrics {
        raw: 0.0,
        spectral: 0.81,
        rms_ratio: 0.95,
        mid: 0.792,
        side: 0.35,
    };
    let weaker_metrics = FixedTrustedWindowMetrics {
        raw: 0.0,
        spectral: 0.648,
        rms_ratio: 0.874,
        mid: 0.6336,
        side: 0.21,
    };

    let stronger_score = normalized_section_mono_score(stronger_metrics, stronger_self);
    let weaker_score = normalized_section_mono_score(weaker_metrics, weaker_self);
    assert!(
        (stronger_score - weaker_score).abs() < 0.02,
        "expected mono normalization to treat equal relative fit similarly: {:.4} vs {:.4}",
        stronger_score,
        weaker_score
    );
}

#[test]
fn mono_first_section_self_consistency_weight_penalizes_weak_mono_more_than_side() {
    let stable = ReferenceConsistencyWindow {
        score: 0.84,
        prior_start: 0,
        current_start: 8,
        len: 8,
        left_spectral: 0.86,
        mid_spectral: 0.84,
        side_spectral: 0.80,
        rms_ratio: 0.98,
    };
    let weak_side = ReferenceConsistencyWindow {
        side_spectral: 0.20,
        ..stable
    };
    let weak_mono = ReferenceConsistencyWindow {
        left_spectral: 0.54,
        mid_spectral: 0.52,
        ..stable
    };

    let stable_weight = mono_first_section_self_consistency_weight(stable);
    let weak_side_weight = mono_first_section_self_consistency_weight(weak_side);
    let weak_mono_weight = mono_first_section_self_consistency_weight(weak_mono);
    assert!(
        stable_weight > weak_side_weight && weak_side_weight > weak_mono_weight,
        "expected mono-first weighting to punish weak left/mid harder than weak side: stable {:.4}, weak_side {:.4}, weak_mono {:.4}",
        stable_weight,
        weak_side_weight,
        weak_mono_weight
    );
}

#[test]
fn mono_first_consensus_prefers_reliable_mono_over_side_flattery() {
    let reliable_reference = TrustedReferenceMetrics {
        env_corr: 0.5,
        local_env_corr: 0.74,
        trusted_score: 0.92,
        trusted_self_left: 0.92,
        trusted_self_mid: 0.93,
        trusted_self_side: 0.82,
        trusted_self_rms: 0.99,
        trusted_emu_raw: 0.02,
        trusted_emu_spectral: 0.90,
        trusted_emu_rms: 0.99,
        trusted_emu_mid: 0.91,
        trusted_emu_side: 0.44,
    };
    let noisy_reference = TrustedReferenceMetrics {
        env_corr: 0.3,
        local_env_corr: 0.46,
        trusted_score: 0.76,
        trusted_self_left: 0.78,
        trusted_self_mid: 0.77,
        trusted_self_side: 0.42,
        trusted_self_rms: 0.86,
        trusted_emu_raw: 0.01,
        trusted_emu_spectral: 0.70,
        trusted_emu_rms: 1.08,
        trusted_emu_mid: 0.71,
        trusted_emu_side: 0.18,
    };
    let candidate_a = [
        MonoFirstSectionedReferenceScore {
            mono_score: 0.90,
            side_score: 0.34,
            combined_score: 0.8160,
            section_reliability: 0.88,
            worst_mono_section_score: 0.74,
            worst_side_section_score: 0.16,
        },
        MonoFirstSectionedReferenceScore {
            mono_score: 0.73,
            side_score: 0.12,
            combined_score: 0.6385,
            section_reliability: 0.36,
            worst_mono_section_score: 0.20,
            worst_side_section_score: 0.04,
        },
    ];
    let candidate_b = [
        MonoFirstSectionedReferenceScore {
            mono_score: 0.84,
            side_score: 0.56,
            combined_score: 0.7980,
            section_reliability: 0.88,
            worst_mono_section_score: 0.66,
            worst_side_section_score: 0.26,
        },
        MonoFirstSectionedReferenceScore {
            mono_score: 0.71,
            side_score: 0.34,
            combined_score: 0.6545,
            section_reliability: 0.36,
            worst_mono_section_score: 0.18,
            worst_side_section_score: 0.10,
        },
    ];

    let score_a = score_mono_first_consensus_candidate(
        "candidate_a",
        &[reliable_reference, noisy_reference],
        &candidate_a,
    );
    let score_b = score_mono_first_consensus_candidate(
        "candidate_b",
        &[reliable_reference, noisy_reference],
        &candidate_b,
    );
    assert!(
        score_a.final_score > score_b.final_score,
        "expected mono-first weighting to favor the candidate that wins on the reliable mono content: {:.4} vs {:.4}",
        score_a.final_score,
        score_b.final_score
    );
}

#[test]
fn mono_consensus_reference_weight_penalizes_weak_mono_more_than_side() {
    let stable = TrustedWindowSelection {
        env_corr: 0.5,
        local_env_corr: 0.72,
        trusted_score: 0.90,
        trusted_self_left: 0.90,
        trusted_self_mid: 0.88,
        trusted_self_side: 0.80,
        trusted_self_rms: 0.99,
        trusted_prior_start: 0,
        trusted_ref_start: 100,
        trusted_emu_start: 120,
        trusted_len: 44_100,
    };
    let weak_side = TrustedWindowSelection {
        trusted_self_side: 0.20,
        ..stable
    };
    let weak_mono = TrustedWindowSelection {
        trusted_self_left: 0.58,
        trusted_self_mid: 0.56,
        ..stable
    };

    let stable_weight = mono_consensus_reference_weight(stable);
    let weak_side_weight = mono_consensus_reference_weight(weak_side);
    let weak_mono_weight = mono_consensus_reference_weight(weak_mono);
    assert!(
        stable_weight > weak_side_weight && weak_side_weight > weak_mono_weight,
        "expected mono-consensus weighting to punish weak mono more than weak side: stable {:.4}, weak_side {:.4}, weak_mono {:.4}",
        stable_weight,
        weak_side_weight,
        weak_mono_weight
    );
}

#[test]
fn resolve_ghz_references_prefers_manifest_order_and_skips_disabled() {
    let manifest = parse_ghz_reference_manifest(
        r#"{
            "references": [
                { "filename": "sonic_ghz_b.flac", "mono_weight": 0.8, "side_weight": 0.4 },
                { "filename": "sonic_ghz_disabled.flac", "enabled": false },
                { "filename": "sonic_ghz_a.flac", "mono_weight": 1.1, "side_weight": 0.9 }
            ]
        }"#,
    )
    .expect("expected manifest");
    let discovered = vec![
        PathBuf::from("/tmp/sonic_ghz_a.flac"),
        PathBuf::from("/tmp/sonic_ghz_disabled.flac"),
        PathBuf::from("/tmp/sonic_ghz_b.flac"),
        PathBuf::from("/tmp/sonic_ghz_extra.flac"),
    ];

    let resolved = resolve_ghz_references(&discovered, Some(&manifest));
    let names: Vec<_> = resolved
        .iter()
        .filter_map(|reference| {
            reference
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        })
        .collect();

    assert_eq!(
        names,
        vec![
            "sonic_ghz_b.flac".to_owned(),
            "sonic_ghz_a.flac".to_owned(),
            "sonic_ghz_extra.flac".to_owned(),
        ]
    );
    assert!((resolved[0].mono_weight - 0.8).abs() < 1e-6);
    assert!((resolved[0].side_weight - 0.4).abs() < 1e-6);
    assert!((resolved[1].mono_weight - 1.1).abs() < 1e-6);
    assert!((resolved[1].side_weight - 0.9).abs() < 1e-6);
}

#[test]
fn resolve_ghz_references_defaults_unlisted_weights_to_one() {
    let manifest = parse_ghz_reference_manifest(
        r#"{
            "references": [
                { "filename": "sonic_ghz_primary.flac", "mono_weight": 0.75, "side_weight": 0.5 }
            ]
        }"#,
    )
    .expect("expected manifest");
    let discovered = vec![
        PathBuf::from("/tmp/sonic_ghz_primary.flac"),
        PathBuf::from("/tmp/sonic_ghz_unlisted.flac"),
    ];

    let resolved = resolve_ghz_references(&discovered, Some(&manifest));
    assert_eq!(resolved.len(), 2);
    assert!((resolved[0].mono_weight - 0.75).abs() < 1e-6);
    assert!((resolved[0].side_weight - 0.5).abs() < 1e-6);
    assert!((resolved[1].mono_weight - 1.0).abs() < 1e-6);
    assert!((resolved[1].side_weight - 1.0).abs() < 1e-6);
}

#[test]
fn parse_ghz_reference_manifest_sanitizes_bad_weights() {
    let manifest = parse_ghz_reference_manifest(
        r#"{
            "references": [
                { "filename": "sonic_ghz_primary.flac", "mono_weight": -2.0, "side_weight": 0.5 },
                { "filename": "sonic_ghz_secondary.flac", "side_weight": -1.0 }
            ]
        }"#,
    )
    .expect("expected manifest");

    assert_eq!(manifest.references[0].mono_weight, 0.0);
    assert_eq!(manifest.references[0].side_weight, 0.5);
    assert_eq!(manifest.references[1].mono_weight, 1.0);
    assert_eq!(manifest.references[1].side_weight, 0.0);
}

#[test]
fn ghz_reference_weight_with_side_overrides_only_changes_targeted_side_weight() {
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert("sonic_ghz_16bap.flac".to_owned(), 0.25);

    let primary = ghz_reference_weight_with_side_overrides(Path::new("sonic_ghz.flac"), &overrides);
    let secondary =
        ghz_reference_weight_with_side_overrides(Path::new("sonic_ghz_16bap.flac"), &overrides);

    assert_eq!(
        primary,
        GhzReferenceWeight {
            mono: 1.0,
            side: 1.0
        }
    );
    assert_eq!(
        secondary,
        GhzReferenceWeight {
            mono: 0.95,
            side: 0.25
        }
    );
}

#[test]
fn summarize_reference_authority_tracks_manual_and_effective_weights() {
    let loaded_refs = vec![
        (PathBuf::from("/tmp/sonic_ghz.flac"), vec![]),
        (PathBuf::from("/tmp/sonic_ghz_16bap.flac"), vec![]),
    ];
    let mono_refs = vec![
        FixedMonoConsensusReference {
            name: "sonic_ghz.flac".to_owned(),
            ref_index: 0,
            selection: TrustedWindowSelection {
                env_corr: 0.0,
                local_env_corr: 0.0,
                trusted_score: 0.0,
                trusted_self_left: 0.0,
                trusted_self_mid: 0.0,
                trusted_self_side: 0.0,
                trusted_self_rms: 1.0,
                trusted_prior_start: 0,
                trusted_ref_start: 0,
                trusted_emu_start: 0,
                trusted_len: 44_100,
            },
            weight: 0.90,
            side_weight: 0.75,
        },
        FixedMonoConsensusReference {
            name: "sonic_ghz_16bap.flac".to_owned(),
            ref_index: 1,
            selection: TrustedWindowSelection {
                env_corr: 0.0,
                local_env_corr: 0.0,
                trusted_score: 0.0,
                trusted_self_left: 0.0,
                trusted_self_mid: 0.0,
                trusted_self_side: 0.0,
                trusted_self_rms: 1.0,
                trusted_prior_start: 0,
                trusted_ref_start: 0,
                trusted_emu_start: 0,
                trusted_len: 44_100,
            },
            weight: 0.50,
            side_weight: 0.30,
        },
    ];
    let side_targets = vec![FixedSideConsensusSectionTarget {
        start: 0,
        len: 44_100,
        target: SideConsensusTarget {
            spectrum: vec![0.0],
            ratio: 0.1,
            self_spectral: 1.0,
            self_ratio_fit: 1.0,
        },
        refs: vec![
            FixedSideConsensusSectionRef {
                ref_index: 0,
                ref_start: 0,
                emu_start: 0,
                weight: 0.80,
            },
            FixedSideConsensusSectionRef {
                ref_index: 1,
                ref_start: 0,
                emu_start: 0,
                weight: 0.40,
            },
        ],
        weight: 0.5,
        transient_reliability: 1.0,
    }];
    let dynamics_targets = vec![FixedSideDynamicsSectionTarget {
        start: 0,
        len: 44_100,
        target: SideDynamicsTarget {
            envelope: vec![0.0],
            ratio: 0.1,
            self_envelope: 1.0,
            self_ratio_fit: 1.0,
        },
        refs: vec![
            FixedSideConsensusSectionRef {
                ref_index: 0,
                ref_start: 0,
                emu_start: 0,
                weight: 0.70,
            },
            FixedSideConsensusSectionRef {
                ref_index: 1,
                ref_start: 0,
                emu_start: 0,
                weight: 0.20,
            },
        ],
        weight: 0.25,
    }];

    let summaries =
        summarize_reference_authority(&loaded_refs, &mono_refs, &side_targets, &dynamics_targets);

    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].name, "sonic_ghz.flac");
    assert!((summaries[0].mono_manual_weight - 1.0).abs() < 1e-6);
    assert!((summaries[0].side_manual_weight - 1.0).abs() < 1e-6);
    assert!((summaries[0].mono_effective_weight - 0.90).abs() < 1e-6);
    assert!((summaries[0].side_consensus_effective_weight - 0.80).abs() < 1e-6);
    assert!((summaries[0].side_dynamics_effective_weight - 0.70).abs() < 1e-6);
    assert_eq!(summaries[1].name, "sonic_ghz_16bap.flac");
    assert!((summaries[1].mono_manual_weight - 0.95).abs() < 1e-6);
    assert!((summaries[1].side_manual_weight - 0.0).abs() < 1e-6);
    assert!((summaries[1].mono_effective_weight - 0.50).abs() < 1e-6);
    assert!((summaries[1].side_consensus_effective_weight - 0.40).abs() < 1e-6);
    assert!((summaries[1].side_dynamics_effective_weight - 0.20).abs() < 1e-6);
}

#[test]
fn build_mono_consensus_target_respects_reference_weights() {
    let windows = [
        MonoConsensusWindow {
            spectrum: vec![1.0, 0.0, -1.0],
            rms: 1.0,
            weight: 3.0,
        },
        MonoConsensusWindow {
            spectrum: vec![0.0, 1.0, -1.0],
            rms: 0.5,
            weight: 1.0,
        },
    ];

    let target = build_mono_consensus_target(&windows).expect("expected mono consensus target");
    assert!(
        target.spectrum[0] > target.spectrum[1],
        "expected heavier first reference to pull target spectrum toward bin 0: {:?}",
        target.spectrum
    );
    assert!(
        (target.rms - 0.875).abs() < 1e-4,
        "expected weighted RMS average, got {:.6}",
        target.rms
    );
}

#[test]
fn mono_consensus_candidate_prefers_weighted_target_match() {
    let target = build_mono_consensus_target(&[
        MonoConsensusWindow {
            spectrum: vec![0.9, 0.3, -1.2],
            rms: 0.95,
            weight: 2.0,
        },
        MonoConsensusWindow {
            spectrum: vec![0.8, 0.4, -1.2],
            rms: 1.05,
            weight: 1.0,
        },
    ])
    .expect("expected mono consensus target");

    let good = [
        MonoConsensusWindow {
            spectrum: vec![0.88, 0.32, -1.2],
            rms: 0.97,
            weight: 2.0,
        },
        MonoConsensusWindow {
            spectrum: vec![0.82, 0.38, -1.2],
            rms: 1.03,
            weight: 1.0,
        },
    ];
    let bad = [
        MonoConsensusWindow {
            spectrum: vec![0.35, 0.85, -1.2],
            rms: 0.97,
            weight: 2.0,
        },
        MonoConsensusWindow {
            spectrum: vec![0.30, 0.90, -1.2],
            rms: 1.03,
            weight: 1.0,
        },
    ];

    let good_score = score_mono_consensus_candidate_windows("good", &good, &target);
    let bad_score = score_mono_consensus_candidate_windows("bad", &bad, &target);
    assert!(
        good_score.final_score > bad_score.final_score,
        "expected candidate closer to weighted mono target to win: {:.4} vs {:.4}",
        good_score.final_score,
        bad_score.final_score
    );
}

#[test]
fn build_side_consensus_target_downweights_side_outlier() {
    let stable_a = SideConsensusWindow {
        spectrum: vec![1.0, 0.2, -0.8],
        ratio: 0.18,
        weight: 1.0,
    };
    let stable_b = SideConsensusWindow {
        spectrum: vec![0.9, 0.3, -0.8],
        ratio: 0.22,
        weight: 1.0,
    };
    let outlier = SideConsensusWindow {
        spectrum: vec![-0.6, 0.1, 1.0],
        ratio: 0.75,
        weight: 1.0,
    };

    let target =
        build_side_consensus_target(&[stable_a.clone(), stable_b.clone(), outlier.clone()])
            .expect("expected side consensus target");

    let stable_similarity = cosine_similarity(&stable_a.spectrum, &target.spectrum);
    let outlier_similarity = cosine_similarity(&outlier.spectrum, &target.spectrum);
    assert!(
        stable_similarity > outlier_similarity,
        "expected stable side references to dominate outlier: stable {:.4} vs outlier {:.4}",
        stable_similarity,
        outlier_similarity
    );
    assert!(
        (target.ratio - 0.20).abs() < (target.ratio - outlier.ratio).abs(),
        "expected side-ratio target to stay near stable pair instead of outlier: target {:.4}",
        target.ratio
    );
}

#[test]
fn side_consensus_candidate_prefers_agreeing_side_window() {
    let target = build_side_consensus_target(&[
        SideConsensusWindow {
            spectrum: vec![0.95, 0.25, -0.8],
            ratio: 0.19,
            weight: 1.0,
        },
        SideConsensusWindow {
            spectrum: vec![0.90, 0.30, -0.8],
            ratio: 0.21,
            weight: 1.0,
        },
    ])
    .expect("expected side consensus target");

    let good = [
        SideConsensusWindow {
            spectrum: vec![0.94, 0.26, -0.8],
            ratio: 0.20,
            weight: 1.0,
        },
        SideConsensusWindow {
            spectrum: vec![0.89, 0.31, -0.8],
            ratio: 0.22,
            weight: 1.0,
        },
    ];
    let bad = [
        SideConsensusWindow {
            spectrum: vec![-0.5, 0.2, 0.95],
            ratio: 0.60,
            weight: 1.0,
        },
        SideConsensusWindow {
            spectrum: vec![-0.4, 0.3, 0.90],
            ratio: 0.55,
            weight: 1.0,
        },
    ];

    let good_score = score_side_consensus_candidate_windows("good", &good, &target);
    let bad_score = score_side_consensus_candidate_windows("bad", &bad, &target);
    assert!(
        good_score.final_score > bad_score.final_score,
        "expected candidate closer to side target to win: {:.4} vs {:.4}",
        good_score.final_score,
        bad_score.final_score
    );
}

#[test]
fn aggregate_side_consensus_windows_respects_weights() {
    let windows = [
        SideConsensusWindow {
            spectrum: vec![1.0, 0.0, -1.0],
            ratio: 0.20,
            weight: 3.0,
        },
        SideConsensusWindow {
            spectrum: vec![0.0, 1.0, -1.0],
            ratio: 0.60,
            weight: 1.0,
        },
    ];

    let aggregated = aggregate_side_consensus_windows(&windows).expect("expected aggregate");
    assert!(
        aggregated.spectrum[0] > aggregated.spectrum[1],
        "expected heavier first side window to dominate spectrum: {:?}",
        aggregated.spectrum
    );
    assert!(
        (aggregated.ratio - 0.30).abs() < 1e-4,
        "expected weighted side ratio average, got {:.6}",
        aggregated.ratio
    );
}

#[test]
fn build_side_dynamics_target_downweights_envelope_outlier() {
    let stable_a = SideDynamicsWindow {
        envelope: vec![0.1, 0.8, 0.2, 0.7],
        ratio: 0.18,
        weight: 1.0,
    };
    let stable_b = SideDynamicsWindow {
        envelope: vec![0.2, 0.7, 0.3, 0.6],
        ratio: 0.22,
        weight: 1.0,
    };
    let outlier = SideDynamicsWindow {
        envelope: vec![0.9, 0.1, 0.8, 0.1],
        ratio: 0.75,
        weight: 1.0,
    };

    let target = build_side_dynamics_target(&[stable_a.clone(), stable_b.clone(), outlier.clone()])
        .expect("expected side dynamics target");

    let stable_similarity = cross_correlation(&stable_a.envelope, &target.envelope);
    let outlier_similarity = cross_correlation(&outlier.envelope, &target.envelope);
    assert!(
        stable_similarity > outlier_similarity,
        "expected stable side envelopes to dominate outlier: stable {:.4} vs outlier {:.4}",
        stable_similarity,
        outlier_similarity
    );
    assert!(
        (target.ratio - 0.20).abs() < (target.ratio - outlier.ratio).abs(),
        "expected side-dynamics ratio target to stay near stable pair instead of outlier: target {:.4}",
        target.ratio
    );
}

#[test]
fn side_dynamics_candidate_prefers_matching_side_shape() {
    let target = build_side_dynamics_target(&[
        SideDynamicsWindow {
            envelope: vec![0.1, 0.8, 0.2, 0.7],
            ratio: 0.19,
            weight: 1.0,
        },
        SideDynamicsWindow {
            envelope: vec![0.2, 0.7, 0.3, 0.6],
            ratio: 0.21,
            weight: 1.0,
        },
    ])
    .expect("expected side dynamics target");

    let good = [
        SideDynamicsWindow {
            envelope: vec![0.1, 0.78, 0.24, 0.68],
            ratio: 0.20,
            weight: 1.0,
        },
        SideDynamicsWindow {
            envelope: vec![0.2, 0.72, 0.28, 0.58],
            ratio: 0.22,
            weight: 1.0,
        },
    ];
    let bad = [
        SideDynamicsWindow {
            envelope: vec![0.7, 0.2, 0.7, 0.2],
            ratio: 0.20,
            weight: 1.0,
        },
        SideDynamicsWindow {
            envelope: vec![0.6, 0.3, 0.6, 0.3],
            ratio: 0.22,
            weight: 1.0,
        },
    ];

    let good_score = score_side_dynamics_candidate_windows("good", &good, &target);
    let bad_score = score_side_dynamics_candidate_windows("bad", &bad, &target);
    assert!(
        good_score.final_score > bad_score.final_score,
        "expected candidate closer to side dynamics target to win: {:.4} vs {:.4}",
        good_score.final_score,
        bad_score.final_score
    );
}

#[test]
fn aggregate_side_dynamics_windows_respects_weights() {
    let windows = [
        SideDynamicsWindow {
            envelope: vec![1.0, 0.0, 0.0],
            ratio: 0.20,
            weight: 3.0,
        },
        SideDynamicsWindow {
            envelope: vec![0.0, 1.0, 0.0],
            ratio: 0.60,
            weight: 1.0,
        },
    ];

    let aggregated = aggregate_side_dynamics_windows(&windows).expect("expected aggregate");
    assert!(
        aggregated.envelope[0] > aggregated.envelope[1],
        "expected heavier first side-dynamics window to dominate envelope: {:?}",
        aggregated.envelope
    );
    assert!(
        (aggregated.ratio - 0.30).abs() < 1e-4,
        "expected weighted side-dynamics ratio average, got {:.6}",
        aggregated.ratio
    );
}

#[test]
fn best_envelope_offset_bins_prefers_zero_for_identical_sequences() {
    let envelope = vec![0.1, 0.5, 0.2, 0.7, 0.3, 0.6];
    let (offset, corr) =
        best_envelope_offset_bins(&envelope, &envelope, 2).expect("expected best offset");
    assert_eq!(
        offset, 0,
        "expected identical envelopes to prefer zero offset"
    );
    assert!(
        corr > 0.99,
        "expected identical envelopes to correlate strongly, got {corr:.4}"
    );
}

#[test]
fn best_envelope_offset_bins_recovers_small_positive_shift() {
    let candidate = vec![0.1, 0.5, 0.2, 0.7, 0.3, 0.6];
    let target = vec![0.0, 0.1, 0.5, 0.2, 0.7, 0.3];
    let (offset, corr) =
        best_envelope_offset_bins(&candidate, &target, 2).expect("expected best offset");
    assert_eq!(
        offset, -1,
        "expected one-bin target delay to be recovered as candidate-leading offset, got {offset}"
    );
    assert!(
        corr > 0.99,
        "expected shifted envelopes to realign strongly, got {corr:.4}"
    );
}

#[test]
fn envelope_transient_profile_reports_first_differences() {
    let envelope = [0.1, 0.5, 0.2, 0.8];
    let transient = envelope_transient_profile(&envelope);
    assert_eq!(transient, vec![0.4, -0.3, 0.6]);
}

#[test]
fn correlation_at_offset_bins_improves_for_shifted_transients() {
    let candidate = vec![0.4, -0.3, 0.6, -0.2];
    let target = vec![0.0, 0.4, -0.3, 0.6];
    let nominal = correlation_at_offset_bins(&candidate, &target, 0);
    let shifted = correlation_at_offset_bins(&candidate, &target, -1);
    assert!(
        shifted > nominal,
        "expected one-bin correction to improve transient correlation: nominal {nominal:.4}, shifted {shifted:.4}"
    );
}

#[test]
fn analyze_transient_alignment_matches_identical_envelopes() {
    let envelope = vec![0.1, 0.5, 0.2, 0.8, 0.3, 0.7];
    let metrics =
        analyze_transient_alignment(&envelope, &envelope, 2).expect("expected alignment metrics");
    assert_eq!(
        metrics.lag_bins, 0,
        "identical envelopes should not need lag"
    );
    assert!(
        metrics.envelope_corr > 0.99
            && metrics.adjusted_envelope_corr > 0.99
            && metrics.transient_corr > 0.99
            && metrics.adjusted_transient_corr > 0.99,
        "expected strong identical alignment, got {metrics:?}"
    );
    assert!(
        (metrics.transient_rms_ratio - 1.0).abs() < 1e-6,
        "expected identical transient RMS, got {:.6}",
        metrics.transient_rms_ratio
    );
}

#[test]
fn analyze_transient_alignment_recovers_small_shift() {
    let candidate = vec![0.0, 0.2, 0.6, 0.1, 0.7, 0.3];
    let target = vec![0.0, 0.0, 0.2, 0.6, 0.1, 0.7];
    let metrics =
        analyze_transient_alignment(&candidate, &target, 2).expect("expected alignment metrics");
    assert_eq!(
        metrics.lag_bins, -1,
        "expected one-bin target delay to be recovered, got {metrics:?}"
    );
    assert!(
        metrics.adjusted_envelope_corr > metrics.envelope_corr
            && metrics.adjusted_transient_corr > metrics.transient_corr,
        "expected lag-adjusted alignment to improve both envelope and transient fits, got {metrics:?}"
    );
}

#[test]
fn summarize_reference_side_dynamics_pairs_reports_single_pair() {
    let pairs = vec![ReferenceSideDynamicsPairAnalysis {
        left_name: "a.flac".to_owned(),
        right_name: "b.flac".to_owned(),
        weight: 0.75,
        left_ratio: 0.12,
        right_ratio: 0.18,
        metrics: TransientAlignmentMetrics {
            envelope_corr: 0.40,
            lag_bins: -1,
            adjusted_envelope_corr: 0.65,
            transient_corr: 0.10,
            adjusted_transient_corr: 0.30,
            transient_rms_ratio: 0.50,
        },
    }];
    let summary =
        summarize_reference_side_dynamics_pairs(&pairs).expect("expected reference summary");
    assert_eq!(summary.pair_count, 1);
    assert!(
        (summary.average_envelope_corr - 0.40).abs() < 1e-6
            && (summary.average_adjusted_envelope_corr - 0.65).abs() < 1e-6
            && (summary.average_transient_corr - 0.10).abs() < 1e-6
            && (summary.average_adjusted_transient_corr - 0.30).abs() < 1e-6
            && (summary.average_abs_lag_bins - 1.0).abs() < 1e-6,
        "unexpected summarized reference metrics: {summary:?}"
    );
}

#[test]
fn summarize_reference_side_consensus_pairs_reports_single_pair() {
    let pairs = vec![ReferenceSideConsensusPairAnalysis {
        weight: 0.75,
        spectral_similarity: 0.40,
        ratio_fit: 0.83,
    }];
    let summary =
        summarize_reference_side_consensus_pairs(&pairs).expect("expected reference summary");
    assert_eq!(summary.pair_count, 1);
    assert!(
        (summary.average_spectral_similarity - 0.40).abs() < 1e-6
            && (summary.average_ratio_fit - 0.83).abs() < 1e-6
            && (summary.worst_spectral_similarity - 0.40).abs() < 1e-6,
        "unexpected summarized reference metrics: {summary:?}"
    );
}

#[test]
fn dominant_pan_behavior_score_prefers_better_mono_guardrail() {
    let safer = dominant_pan_behavior_score(0.9200, 0.1400, 0.1800);
    let flashier = dominant_pan_behavior_score(0.9050, 0.2500, 0.2600);
    assert!(
        safer > flashier,
        "expected better mono guardrail to dominate side flattery: safer {safer:.6}, flashier {flashier:.6}"
    );
}

#[test]
fn dominant_pan_behavior_score_uses_side_as_tiebreaker() {
    let current = dominant_pan_behavior_score(0.9200, 0.1350, 0.1800);
    let candidate = dominant_pan_behavior_score(0.9200, 0.1550, 0.1950);
    assert!(
        candidate > current,
        "expected better dominant side sections to break mono tie: current {current:.6}, candidate {candidate:.6}"
    );
}

#[test]
fn summarize_ym_pan_states_tracks_pre_interval_state_and_changes() {
    let writes = [
        TimedYm2612Write {
            master_tick: 10,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0xB4,
            value: 0xC0,
        },
        TimedYm2612Write {
            master_tick: 40,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0xB4,
            value: 0x80,
        },
        TimedYm2612Write {
            master_tick: 70,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0xB4,
            value: 0x00,
        },
    ];

    let summary = summarize_ym_pan_states(&writes, 20, 100);

    assert_eq!(summary[0].stereo_ticks, 20);
    assert_eq!(summary[0].left_ticks, 30);
    assert_eq!(summary[0].off_ticks, 30);
    assert_eq!(summary[0].right_ticks, 0);
    assert_eq!(summary[0].change_count, 2);
}

#[test]
fn summarize_ym_pan_states_maps_port_one_channels_correctly() {
    let writes = [
        TimedYm2612Write {
            master_tick: 5,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB5,
            value: 0x40,
        },
        TimedYm2612Write {
            master_tick: 25,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB5,
            value: 0xC0,
        },
    ];

    let summary = summarize_ym_pan_states(&writes, 0, 50);

    assert_eq!(summary[4].right_ticks, 20);
    assert_eq!(summary[4].stereo_ticks, 25);
    assert_eq!(summary[4].off_ticks, 5);
    assert_eq!(summary[4].change_count, 2);
    assert_eq!(summary[0].off_ticks, 50);
}

#[test]
fn last_ym_pan_changes_before_returns_most_recent_real_change() {
    let writes = [
        TimedYm2612Write {
            master_tick: 10,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0xB4,
            value: 0xC0,
        },
        TimedYm2612Write {
            master_tick: 20,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0xB4,
            value: 0xC0,
        },
        TimedYm2612Write {
            master_tick: 35,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0xB4,
            value: 0x80,
        },
    ];

    let changes = last_ym_pan_changes_before(&writes, 40);

    assert_eq!(changes[0], Some((35, YmPanState::Left)));
}

#[test]
fn last_ym_pan_changes_before_maps_port_one_channels_correctly() {
    let writes = [TimedYm2612Write {
        master_tick: 12,
        frame: 0,
        scanline: 0,
        port: 1,
        addr: 0xB6,
        value: 0x40,
    }];

    let changes = last_ym_pan_changes_before(&writes, 50);

    assert_eq!(changes[5], Some((12, YmPanState::Right)));
    assert_eq!(changes[0], None);
}

#[test]
fn scale_side_channel_one_is_identity() {
    let stereo = [1.0, -0.5, 0.25, 0.75];
    let scaled = scale_side_channel(&stereo, 1.0);
    assert_eq!(scaled, stereo);
}

#[test]
fn scale_side_channel_zero_collapses_to_mono() {
    let stereo = [1.0, 0.0, 0.5, -0.5];
    let scaled = scale_side_channel(&stereo, 0.0);
    assert_eq!(scaled, vec![0.5, 0.5, 0.0, 0.0]);
}

#[test]
fn force_centered_pan_writes_sets_lr_bits_on_pan_registers() {
    let writes = [
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0xB4,
            value: 0x05,
        },
        TimedYm2612Write {
            master_tick: 1,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB6,
            value: 0x20,
        },
        TimedYm2612Write {
            master_tick: 2,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x30,
            value: 0x7F,
        },
    ];

    let centered = force_centered_pan_writes(&writes);

    assert_eq!(centered[0].value, 0xC5);
    assert_eq!(centered[1].value, 0xE0);
    assert_eq!(centered[2].value, 0x7F);
}

#[test]
fn force_centered_pan_writes_before_only_mutates_earlier_pan_writes() {
    let writes = [
        TimedYm2612Write {
            master_tick: 5,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0xB4,
            value: 0x01,
        },
        TimedYm2612Write {
            master_tick: 15,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0xB4,
            value: 0x02,
        },
    ];

    let centered = force_centered_pan_writes_before(&writes, 10);

    assert_eq!(centered[0].value, 0xC1);
    assert_eq!(centered[1].value, 0x02);
}

#[test]
fn force_centered_pan_writes_for_channels_only_mutates_selected_channels() {
    let writes = [
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0xB4,
            value: 0x01,
        },
        TimedYm2612Write {
            master_tick: 1,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB4,
            value: 0x02,
        },
        TimedYm2612Write {
            master_tick: 2,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB5,
            value: 0x20,
        },
        TimedYm2612Write {
            master_tick: 3,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x30,
            value: 0x7F,
        },
    ];

    let centered = force_centered_pan_writes_for_channels(&writes, (1 << 3) | (1 << 4));

    assert_eq!(centered[0].value, 0x01);
    assert_eq!(centered[1].value, 0xC2);
    assert_eq!(centered[2].value, 0xE0);
    assert_eq!(centered[3].value, 0x7F);
}

#[test]
fn force_centered_pan_writes_for_channels_before_respects_cutoff_and_channel() {
    let writes = [
        TimedYm2612Write {
            master_tick: 5,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB4,
            value: 0x01,
        },
        TimedYm2612Write {
            master_tick: 5,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB5,
            value: 0x02,
        },
        TimedYm2612Write {
            master_tick: 15,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB4,
            value: 0x04,
        },
    ];

    let centered = force_centered_pan_writes_for_channels_before(&writes, 10, 1 << 3);

    assert_eq!(centered[0].value, 0xC1);
    assert_eq!(centered[1].value, 0x02);
    assert_eq!(centered[2].value, 0x04);
}

#[test]
fn force_centered_pan_writes_for_channels_between_respects_window_and_channel() {
    let writes = [
        TimedYm2612Write {
            master_tick: 4,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB4,
            value: 0x01,
        },
        TimedYm2612Write {
            master_tick: 5,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB4,
            value: 0x02,
        },
        TimedYm2612Write {
            master_tick: 6,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB5,
            value: 0x20,
        },
        TimedYm2612Write {
            master_tick: 8,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB4,
            value: 0x04,
        },
    ];

    let centered = force_centered_pan_writes_for_channels_between(&writes, 5, 8, 1 << 3);

    assert_eq!(centered[0].value, 0x01);
    assert_eq!(centered[1].value, 0xC2);
    assert_eq!(centered[2].value, 0x20);
    assert_eq!(centered[3].value, 0x04);
}

#[test]
fn delay_pan_writes_for_channels_shifts_selected_pan_events_only() {
    let writes = [
        TimedYm2612Write {
            master_tick: 5,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0xB4,
            value: 0x01,
        },
        TimedYm2612Write {
            master_tick: 7,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB4,
            value: 0x02,
        },
        TimedYm2612Write {
            master_tick: 9,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x30,
            value: 0x7F,
        },
    ];

    let delayed = delay_pan_writes_for_channels(&writes, 12, 1 << 3);

    assert_eq!(delayed[0].master_tick, 5);
    assert_eq!(delayed[1].master_tick, 9);
    assert_eq!(delayed[2].master_tick, 19);
}

#[test]
fn delay_pan_writes_for_channels_resorts_shifted_events_by_master_tick() {
    let writes = [
        TimedYm2612Write {
            master_tick: 10,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB4,
            value: 0x02,
        },
        TimedYm2612Write {
            master_tick: 15,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x30,
            value: 0x7F,
        },
    ];

    let delayed = delay_pan_writes_for_channels(&writes, 12, 1 << 3);

    assert_eq!(delayed[0].addr, 0x30);
    assert_eq!(delayed[0].master_tick, 15);
    assert_eq!(delayed[1].addr, 0xB4);
    assert_eq!(delayed[1].master_tick, 22);
}

#[test]
fn delay_pan_state_changes_for_channels_shifts_only_real_state_changes() {
    let writes = [
        TimedYm2612Write {
            master_tick: 5,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB4,
            value: 0x80,
        },
        TimedYm2612Write {
            master_tick: 7,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB4,
            value: 0x80,
        },
        TimedYm2612Write {
            master_tick: 9,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB4,
            value: 0xC0,
        },
        TimedYm2612Write {
            master_tick: 11,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x30,
            value: 0x7F,
        },
    ];

    let delayed = delay_pan_state_changes_for_channels(&writes, 12, 1 << 3);

    assert_eq!(delayed[0].master_tick, 7);
    assert_eq!(delayed[0].value, 0x80);
    assert_eq!(delayed[1].master_tick, 11);
    assert_eq!(delayed[1].addr, 0x30);
    assert_eq!(delayed[2].master_tick, 17);
    assert_eq!(delayed[2].value, 0x80);
    assert_eq!(delayed[3].master_tick, 21);
    assert_eq!(delayed[3].value, 0xC0);
}

#[test]
fn delay_pan_state_changes_for_channels_resorts_shifted_events_by_master_tick() {
    let writes = [
        TimedYm2612Write {
            master_tick: 10,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB4,
            value: 0x80,
        },
        TimedYm2612Write {
            master_tick: 15,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x30,
            value: 0x7F,
        },
    ];

    let delayed = delay_pan_state_changes_for_channels(&writes, 12, 1 << 3);

    assert_eq!(delayed[0].addr, 0x30);
    assert_eq!(delayed[0].master_tick, 15);
    assert_eq!(delayed[1].addr, 0xB4);
    assert_eq!(delayed[1].master_tick, 22);
}

#[test]
fn delay_key_writes_for_channels_shifts_selected_key_events_only() {
    let writes = [
        TimedYm2612Write {
            master_tick: 5,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x28,
            value: 0x14,
        },
        TimedYm2612Write {
            master_tick: 7,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x28,
            value: 0x10,
        },
        TimedYm2612Write {
            master_tick: 9,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0xA4,
            value: 0x22,
        },
    ];

    let delayed = delay_key_writes_for_channels(&writes, 12, 1 << 3);

    assert_eq!(delayed[0].master_tick, 7);
    assert_eq!(delayed[0].value, 0x10);
    assert_eq!(delayed[1].master_tick, 9);
    assert_eq!(delayed[2].master_tick, 17);
    assert_eq!(delayed[2].value, 0x14);
}

#[test]
fn delay_key_writes_for_channels_respects_invalid_selector_and_port() {
    let writes = [
        TimedYm2612Write {
            master_tick: 5,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x28,
            value: 0x13,
        },
        TimedYm2612Write {
            master_tick: 6,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0x28,
            value: 0x14,
        },
        TimedYm2612Write {
            master_tick: 7,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x28,
            value: 0x15,
        },
    ];

    let delayed = delay_key_writes_for_channels(&writes, 12, 1 << 4);

    assert_eq!(delayed[0].master_tick, 5);
    assert_eq!(delayed[1].master_tick, 6);
    assert_eq!(delayed[2].master_tick, 19);
    assert_eq!(delayed[2].value, 0x15);
}

#[test]
fn mute_ym_pan_writes_outside_channels_mutes_other_channels_only() {
    let writes = [
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0xB4,
            value: 0x41,
        },
        TimedYm2612Write {
            master_tick: 1,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB4,
            value: 0x82,
        },
        TimedYm2612Write {
            master_tick: 2,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB5,
            value: 0xE0,
        },
        TimedYm2612Write {
            master_tick: 3,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x30,
            value: 0x7F,
        },
    ];

    let masked = mute_ym_pan_writes_outside_channels(&writes, (1 << 0) | (1 << 3));

    assert_eq!(masked[0].value, 0x41);
    assert_eq!(masked[1].value, 0x82);
    assert_eq!(masked[2].value, 0x20);
    assert_eq!(masked[3].value, 0x7F);
}

#[test]
fn suppress_ym_frequency_writes_for_channels_between_filters_selected_channel_in_window() {
    let writes = [
        TimedYm2612Write {
            master_tick: 5,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xA4,
            value: 0x22,
        },
        TimedYm2612Write {
            master_tick: 6,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xA0,
            value: 0x69,
        },
        TimedYm2612Write {
            master_tick: 7,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xA5,
            value: 0x33,
        },
        TimedYm2612Write {
            master_tick: 8,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xA1,
            value: 0x44,
        },
        TimedYm2612Write {
            master_tick: 9,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB4,
            value: 0xC0,
        },
    ];

    let filtered = suppress_ym_frequency_writes_for_channels_between(&writes, 5, 8, 1 << 3);

    assert_eq!(filtered.len(), 3);
    assert_eq!(filtered[0].addr, 0xA5);
    assert_eq!(filtered[1].addr, 0xA1);
    assert_eq!(filtered[2].addr, 0xB4);
}

#[test]
fn suppress_ym_frequency_writes_for_channels_between_respects_window_and_channel() {
    let writes = [
        TimedYm2612Write {
            master_tick: 4,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xA4,
            value: 0x20,
        },
        TimedYm2612Write {
            master_tick: 5,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xA4,
            value: 0x21,
        },
        TimedYm2612Write {
            master_tick: 6,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0xA4,
            value: 0x10,
        },
        TimedYm2612Write {
            master_tick: 8,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xA0,
            value: 0x30,
        },
    ];

    let filtered = suppress_ym_frequency_writes_for_channels_between(&writes, 5, 8, 1 << 3);

    assert_eq!(filtered.len(), 3);
    assert_eq!(filtered[0].master_tick, 4);
    assert_eq!(filtered[1].port, 0);
    assert_eq!(filtered[1].addr, 0xA4);
    assert_eq!(filtered[2].master_tick, 8);
    assert_eq!(filtered[2].addr, 0xA0);
}

#[test]
fn suppress_ym_key_writes_for_channels_between_filters_selected_channel_in_window() {
    let writes = [
        TimedYm2612Write {
            master_tick: 5,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x28,
            value: 0x14,
        },
        TimedYm2612Write {
            master_tick: 6,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x28,
            value: 0x10,
        },
        TimedYm2612Write {
            master_tick: 7,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0x28,
            value: 0x14,
        },
        TimedYm2612Write {
            master_tick: 8,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x28,
            value: 0x14,
        },
    ];

    let filtered = suppress_ym_key_writes_for_channels_between(&writes, 5, 8, 1 << 3);

    assert_eq!(filtered.len(), 3);
    assert_eq!(filtered[0].value, 0x10);
    assert_eq!(filtered[1].port, 1);
    assert_eq!(filtered[2].master_tick, 8);
}

#[test]
fn suppress_ym_key_writes_for_channels_between_ignores_invalid_selector_and_bounds() {
    let writes = [
        TimedYm2612Write {
            master_tick: 4,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x28,
            value: 0x13,
        },
        TimedYm2612Write {
            master_tick: 5,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x28,
            value: 0x15,
        },
        TimedYm2612Write {
            master_tick: 6,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x2A,
            value: 0x7F,
        },
        TimedYm2612Write {
            master_tick: 8,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x28,
            value: 0x15,
        },
    ];

    let filtered = suppress_ym_key_writes_for_channels_between(&writes, 5, 8, 1 << 4);

    assert_eq!(filtered.len(), 3);
    assert_eq!(filtered[0].value, 0x13);
    assert_eq!(filtered[1].addr, 0x2A);
    assert_eq!(filtered[2].master_tick, 8);
}

#[test]
fn ym_channel_stems_sum_back_to_full_ym_render() {
    let end_tick = master_ticks_from_output_samples(4410);
    let writes = vec![
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0xB0,
            value: 0x07,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0xB4,
            value: 0x40,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x40,
            value: 127,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x44,
            value: 127,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x48,
            value: 127,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x4C,
            value: 127,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x30,
            value: 0x01,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x40,
            value: 0,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x50,
            value: 31,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x60,
            value: 0,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x70,
            value: 0,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x80,
            value: 0x0F,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0xA4,
            value: (4 << 3) | 0x02,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0xA0,
            value: 0x8D,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x28,
            value: 0x10,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB0,
            value: 0x07,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xB4,
            value: 0x80,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0x40,
            value: 127,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0x44,
            value: 127,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0x48,
            value: 127,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0x4C,
            value: 127,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0x30,
            value: 0x01,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0x40,
            value: 0,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0x50,
            value: 31,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0x60,
            value: 0,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0x70,
            value: 0,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0x80,
            value: 0x0F,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xA4,
            value: (4 << 3) | 0x02,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 1,
            addr: 0xA0,
            value: 0xA4,
        },
        TimedYm2612Write {
            master_tick: 0,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x28,
            value: 0x14,
        },
    ];

    let config = AudioOutputConfig::legacy().with_psg_gain(0.0);
    let mut full_renderer = CoreAudioRenderer::with_audio_output_config(config);
    let full = full_renderer.render_timed_writes(&writes, &[], 0, end_tick);

    let mut ch1_renderer = CoreAudioRenderer::with_audio_output_config(config);
    let ch1 = ch1_renderer.render_timed_writes(
        &mute_ym_pan_writes_outside_channels(&writes, 1 << 0),
        &[],
        0,
        end_tick,
    );
    let mut ch4_renderer = CoreAudioRenderer::with_audio_output_config(config);
    let ch4 = ch4_renderer.render_timed_writes(
        &mute_ym_pan_writes_outside_channels(&writes, 1 << 3),
        &[],
        0,
        end_tick,
    );
    let summed = sum_stereo_sources(&[&ch1, &ch4]);

    let max_diff = full
        .iter()
        .zip(&summed)
        .map(|(&a, &b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(
        max_diff < 1.0e-6,
        "expected masked channel stems to reconstruct full render, max_diff={max_diff}"
    );
}

#[test]
fn summarize_section_emu_positions_reports_min_mean_max() {
    let refs = vec![
        FixedSideConsensusSectionRef {
            ref_index: 0,
            ref_start: 100,
            emu_start: 1_000,
            weight: 1.0,
        },
        FixedSideConsensusSectionRef {
            ref_index: 1,
            ref_start: 200,
            emu_start: 1_600,
            weight: 1.0,
        },
    ];

    let summary = summarize_section_emu_positions(&refs, 512).expect("expected summary");

    assert_eq!(summary.min_start, 1_000);
    assert_eq!(summary.max_start, 1_600);
    assert_eq!(summary.mean_start, 1_300);
    assert_eq!(summary.max_end, 2_112);
}

#[test]
fn section_sample_range_from_refs_offsets_from_capture_start() {
    let trace = GhzTimedTrace {
        extra_frames: 0,
        capture_start_tick: 0,
        capture_start_sample: 12_000,
        end_tick: 0,
        ym_writes: Vec::new(),
        psg_writes: Vec::new(),
    };
    let refs = vec![
        FixedSideConsensusSectionRef {
            ref_index: 0,
            ref_start: 100,
            emu_start: 1_000,
            weight: 1.0,
        },
        FixedSideConsensusSectionRef {
            ref_index: 1,
            ref_start: 200,
            emu_start: 1_600,
            weight: 1.0,
        },
    ];

    let (positions, start_sample, end_sample) =
        section_sample_range_from_refs(&trace, &refs, 512).expect("expected sample range");

    assert_eq!(positions.min_start, 1_000);
    assert_eq!(positions.max_start, 1_600);
    assert_eq!(positions.mean_start, 1_300);
    assert_eq!(positions.max_end, 2_112);
    assert_eq!(start_sample, 13_000);
    assert_eq!(end_sample, 14_112);
}

#[test]
fn summarize_ym2612_tracked_events_in_sample_range_counts_only_selected_window() {
    let events = vec![
        Ym2612TrackedEvent {
            sample: 9,
            channel: 0,
            kind: Ym2612TrackedEventKind::KeyOn,
            freq_hz: Some(440.0),
        },
        Ym2612TrackedEvent {
            sample: 10,
            channel: 0,
            kind: Ym2612TrackedEventKind::KeyOn,
            freq_hz: Some(440.0),
        },
        Ym2612TrackedEvent {
            sample: 11,
            channel: 0,
            kind: Ym2612TrackedEventKind::ToneChange,
            freq_hz: Some(466.16),
        },
        Ym2612TrackedEvent {
            sample: 12,
            channel: 3,
            kind: Ym2612TrackedEventKind::ToneChange,
            freq_hz: Some(55.0),
        },
        Ym2612TrackedEvent {
            sample: 13,
            channel: 3,
            kind: Ym2612TrackedEventKind::KeyOff,
            freq_hz: None,
        },
        Ym2612TrackedEvent {
            sample: 14,
            channel: 5,
            kind: Ym2612TrackedEventKind::KeyOn,
            freq_hz: Some(220.0),
        },
    ];

    let (counts, total) = summarize_ym2612_tracked_events_in_sample_range(&events, 10, 14);

    assert_eq!(total, 4);
    assert_eq!(
        counts[0][ym2612_event_kind_index(Ym2612TrackedEventKind::KeyOn)],
        1
    );
    assert_eq!(
        counts[0][ym2612_event_kind_index(Ym2612TrackedEventKind::ToneChange)],
        1
    );
    assert_eq!(
        counts[3][ym2612_event_kind_index(Ym2612TrackedEventKind::ToneChange)],
        1
    );
    assert_eq!(
        counts[3][ym2612_event_kind_index(Ym2612TrackedEventKind::KeyOff)],
        1
    );
    assert_eq!(
        counts[5][ym2612_event_kind_index(Ym2612TrackedEventKind::KeyOn)],
        0
    );
}

#[test]
fn summarize_side_dynamics_window_consistency_recovers_single_pair() {
    let windows = vec![
        SideDynamicsWindow {
            envelope: vec![0.0, 0.3, 0.8, 0.2, 0.7, 0.1],
            ratio: 0.18,
            weight: 1.0,
        },
        SideDynamicsWindow {
            envelope: vec![0.0, 0.0, 0.3, 0.8, 0.2, 0.7],
            ratio: 0.11,
            weight: 0.5,
        },
    ];
    let summary = summarize_side_dynamics_window_consistency(&windows, 2)
        .expect("expected side-dynamics window consistency summary");
    assert_eq!(summary.pair_count, 1);
    assert!(
        summary.average_adjusted_envelope_corr > summary.average_envelope_corr
            && summary.average_adjusted_transient_corr > summary.average_transient_corr,
        "expected lag-adjusted summary to improve agreement, got {summary:?}"
    );
}

#[test]
fn reference_side_transient_consistency_weight_penalizes_disagreeing_transients() {
    let stable = ReferenceSideDynamicsConsistency {
        pair_count: 1,
        average_envelope_corr: 0.80,
        average_adjusted_envelope_corr: 0.92,
        average_transient_corr: 0.70,
        average_adjusted_transient_corr: 0.90,
        average_transient_rms_fit: 0.95,
        average_abs_lag_bins: 0.5,
        worst_adjusted_transient_corr: 0.88,
    };
    let unstable = ReferenceSideDynamicsConsistency {
        average_envelope_corr: 0.10,
        average_adjusted_envelope_corr: 0.22,
        average_transient_corr: -0.05,
        average_adjusted_transient_corr: 0.18,
        average_transient_rms_fit: 0.17,
        average_abs_lag_bins: 1.0,
        worst_adjusted_transient_corr: 0.18,
        ..stable
    };
    let stable_weight = reference_side_transient_consistency_weight(stable);
    let unstable_weight = reference_side_transient_consistency_weight(unstable);
    assert!(
        stable_weight > unstable_weight,
        "expected unstable transient target to get less weight: stable {:.4} unstable {:.4}",
        stable_weight,
        unstable_weight
    );
}

#[test]
fn dominant_section_impact_prefers_heavier_section_over_light_catastrophe() {
    let section_finals = [0.05, 0.40];
    let section_weights = [0.02, 0.40];
    let (idx, impact) = dominant_section_impact_index(&section_finals, &section_weights)
        .expect("expected dominant section impact");
    assert_eq!(
        idx, 1,
        "expected heavier section to dominate weighted impact"
    );
    assert!(
        impact > 0.0,
        "expected positive dominant impact, got {impact:.6}"
    );
}

#[test]
fn section_impact_ranking_orders_by_weighted_deficit() {
    let section_finals = [0.05, 0.40, 0.80];
    let section_weights = [0.02, 0.40, 0.80];
    let ranked = section_impact_ranking(&section_finals, &section_weights);
    let ranked_indices: Vec<_> = ranked.iter().map(|(idx, _)| *idx).collect();
    assert_eq!(
        ranked_indices,
        vec![1, 2, 0],
        "expected ranking by weighted impact, not raw weakest score"
    );
    assert!(
        ranked.windows(2).all(|window| window[0].1 >= window[1].1),
        "expected descending impact order: {ranked:?}"
    );
}

#[test]
fn apply_side_transient_mix_zero_amount_is_identity() {
    let stereo = vec![0.2, -0.1, 0.4, 0.3, -0.2, -0.5, 0.1, 0.7];
    let shaped = apply_side_transient_mix(&stereo, 0.0);
    assert_eq!(shaped, stereo);
}

#[test]
fn apply_side_transient_mix_preserves_mono_signal() {
    let stereo = vec![0.2, 0.2, -0.4, -0.4, 0.6, 0.6, -0.1, -0.1];
    let shaped = apply_side_transient_mix(&stereo, 0.35);
    assert_eq!(shaped, stereo);
}

#[test]
fn apply_side_transient_mix_increases_side_transient_rms() {
    let mut stereo = Vec::new();
    for side in [0.0_f32, 0.0, 1.0, 1.0, 1.0, 0.0] {
        stereo.push(side);
        stereo.push(-side);
    }

    let shaped = apply_side_transient_mix(&stereo, 0.5);
    let (_, base_side) = stereo_mid_side(&stereo);
    let (_, shaped_side) = stereo_mid_side(&shaped);
    let base_transient = envelope_transient_profile(&base_side);
    let shaped_transient = envelope_transient_profile(&shaped_side);

    assert!(
        rms(&shaped_transient) > rms(&base_transient),
        "expected transient mix to sharpen pure-side step: base {:.4} shaped {:.4}",
        rms(&base_transient),
        rms(&shaped_transient)
    );
}

#[test]
fn apply_side_delay_mix_zero_amount_is_identity() {
    let stereo = vec![0.2, -0.1, 0.4, 0.3, -0.2, -0.5, 0.1, 0.7];
    let shaped = apply_side_delay_mix(&stereo, 0.0, 1);
    assert_eq!(shaped, stereo);
}

#[test]
fn apply_side_delay_mix_preserves_mono_signal() {
    let stereo = vec![0.2, 0.2, -0.4, -0.4, 0.6, 0.6, -0.1, -0.1];
    let shaped = apply_side_delay_mix(&stereo, 0.35, 2);
    assert_eq!(shaped, stereo);
}

#[test]
fn apply_side_delay_mix_adds_delayed_side_tail() {
    let stereo = vec![1.0, -1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let shaped = apply_side_delay_mix(&stereo, 0.5, 1);
    let (_, shaped_side) = stereo_mid_side(&shaped);
    let expected = [1.0, 0.5, 0.0, 0.0];
    for (&actual, &want) in shaped_side.iter().zip(expected.iter()) {
        assert!(
            (actual - want).abs() < 1e-6,
            "expected delayed side tail {want:.4}, got {actual:.4}"
        );
    }
}

#[test]
fn apply_side_triggered_persistence_mix_zero_amount_is_identity() {
    let stereo = vec![0.2, -0.1, 0.4, 0.3, -0.2, -0.5, 0.1, 0.7];
    let shaped = apply_side_triggered_persistence_mix(&stereo, &[1, 3], 0.0, 8);
    assert_eq!(shaped, stereo);
}

#[test]
fn apply_side_triggered_persistence_mix_preserves_mono_signal() {
    let stereo = vec![0.2, 0.2, -0.4, -0.4, 0.6, 0.6, -0.1, -0.1];
    let shaped = apply_side_triggered_persistence_mix(&stereo, &[0, 2], 0.35, 16);
    assert_eq!(shaped, stereo);
}

#[test]
fn apply_side_triggered_persistence_mix_adds_decay_after_trigger() {
    let stereo = vec![1.0, -1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let shaped = apply_side_triggered_persistence_mix(&stereo, &[0], 0.5, 2);
    let (_, shaped_side) = stereo_mid_side(&shaped);

    assert!(
        shaped_side[0] > 1.0,
        "expected trigger to boost initial side sample, got {:.4}",
        shaped_side[0]
    );
    assert!(
        shaped_side[1] > 0.0,
        "expected decaying side tail after trigger, got {:.4}",
        shaped_side[1]
    );
    assert!(
        shaped_side[2] < shaped_side[1],
        "expected persistence to decay across samples: {:?}",
        shaped_side
    );
}

#[test]
fn apply_side_triggered_delayed_persistence_mix_shifts_boost_to_later_sample() {
    let stereo = vec![1.0, -1.0, 0.5, -0.5, 0.0, 0.0, 0.0, 0.0];
    let shaped = apply_side_triggered_delayed_persistence_mix(&stereo, &[0], 1, 0.5, 2);
    let (_, shaped_side) = stereo_mid_side(&shaped);

    assert!(
        (shaped_side[0] - 1.0).abs() < 1e-6,
        "expected delayed trigger not to change first sample: {:?}",
        shaped_side
    );
    assert!(
        shaped_side[1] > 0.5,
        "expected delayed trigger to boost second sample, got {:.4}",
        shaped_side[1]
    );
}

#[test]
fn apply_side_triggered_transient_mix_zero_amount_is_identity() {
    let stereo = vec![0.2, -0.1, 0.4, 0.3, -0.2, -0.5, 0.1, 0.7];
    let shaped = apply_side_triggered_transient_mix(&stereo, &[1, 3], 2, 0.0);
    assert_eq!(shaped, stereo);
}

#[test]
fn apply_side_triggered_transient_mix_only_sharpens_inside_trigger_window() {
    let stereo = vec![0.0, 0.0, 1.0, -1.0, 0.5, -0.5, 0.0, 0.0];
    let shaped = apply_side_triggered_transient_mix(&stereo, &[1], 2, 0.5);
    let (_, base_side) = stereo_mid_side(&stereo);
    let (_, shaped_side) = stereo_mid_side(&shaped);

    assert!(
        (shaped_side[0] - base_side[0]).abs() < 1e-6,
        "expected no change before trigger window"
    );
    assert!(
        shaped_side[1] > base_side[1],
        "expected transient sharpening inside trigger window"
    );
    assert!(
        (shaped_side[3] - base_side[3]).abs() < 1e-6,
        "expected no change after trigger window"
    );
}

#[test]
fn key_on_trigger_samples_filters_channels_and_capture_offset() {
    let writes = vec![
        TimedYm2612Write {
            master_tick: 100,
            port: 0,
            addr: 0x28,
            value: 0xF0,
            frame: 0,
            scanline: 0,
        },
        TimedYm2612Write {
            master_tick: 200,
            port: 0,
            addr: 0x28,
            value: 0xF4,
            frame: 0,
            scanline: 0,
        },
        TimedYm2612Write {
            master_tick: 300,
            port: 0,
            addr: 0x28,
            value: 0x04,
            frame: 0,
            scanline: 0,
        },
    ];

    let capture_start = samples_from_tick(150);
    let triggers = key_on_trigger_samples(&writes, capture_start, 1 << 3);
    let expected = vec![(samples_from_tick(200) - capture_start) as usize];
    assert_eq!(triggers, expected);
}

#[test]
fn tracked_event_trigger_samples_filters_kind_channel_and_capture_offset() {
    let events = vec![
        Ym2612TrackedEvent {
            sample: 10,
            channel: 0,
            kind: Ym2612TrackedEventKind::ToneChange,
            freq_hz: Some(440.0),
        },
        Ym2612TrackedEvent {
            sample: 20,
            channel: 3,
            kind: Ym2612TrackedEventKind::ToneChange,
            freq_hz: Some(660.0),
        },
        Ym2612TrackedEvent {
            sample: 30,
            channel: 3,
            kind: Ym2612TrackedEventKind::KeyOn,
            freq_hz: Some(660.0),
        },
    ];

    let triggers =
        tracked_event_trigger_samples(&events, 15, Ym2612TrackedEventKind::ToneChange, 1 << 3);
    assert_eq!(triggers, vec![5]);
}

#[test]
fn hybrid_mono_consensus_prefers_better_mono_when_side_delta_is_small() {
    let stronger_mono = ConsensusCandidateScore {
        name: "stronger_mono".to_owned(),
        average_score: 0.9400,
        disagreement_penalty: 0.0020,
        final_score: 0.9380,
    };
    let weaker_mono = ConsensusCandidateScore {
        name: "weaker_mono".to_owned(),
        average_score: 0.9300,
        disagreement_penalty: 0.0010,
        final_score: 0.9290,
    };

    let stronger = score_hybrid_mono_consensus_candidate(
        "stronger",
        stronger_mono,
        &[0.46, 0.48],
        &[1.0, 1.0],
        1.0,
    );
    let weaker = score_hybrid_mono_consensus_candidate(
        "weaker",
        weaker_mono,
        &[0.50, 0.52],
        &[1.0, 1.0],
        1.0,
    );

    assert!(
        stronger.final_score > weaker.final_score,
        "expected stronger mono candidate to survive a small side disadvantage: {:.4} vs {:.4}",
        stronger.final_score,
        weaker.final_score
    );
}

#[test]
fn hybrid_mono_consensus_uses_side_as_tiebreaker_when_mono_is_tied() {
    let mono = ConsensusCandidateScore {
        name: "mono".to_owned(),
        average_score: 0.9380,
        disagreement_penalty: 0.0015,
        final_score: 0.9365,
    };

    let better_side = score_hybrid_mono_consensus_candidate(
        "better_side",
        mono.clone(),
        &[0.50, 0.52],
        &[1.0, 1.0],
        1.0,
    );
    let worse_side =
        score_hybrid_mono_consensus_candidate("worse_side", mono, &[0.42, 0.44], &[1.0, 1.0], 1.0);

    assert!(
        better_side.final_score > worse_side.final_score,
        "expected side to break a mono tie: {:.4} vs {:.4}",
        better_side.final_score,
        worse_side.final_score
    );
}

#[test]
fn hybrid_mono_consensus_downweights_side_when_reliability_is_low() {
    let mono = ConsensusCandidateScore {
        name: "mono".to_owned(),
        average_score: 0.9380,
        disagreement_penalty: 0.0015,
        final_score: 0.9365,
    };

    let high_better_side = score_hybrid_mono_consensus_candidate(
        "high_better_side",
        mono.clone(),
        &[0.50, 0.52],
        &[1.0, 1.0],
        1.0,
    );
    let high_worse_side = score_hybrid_mono_consensus_candidate(
        "high_worse_side",
        mono.clone(),
        &[0.42, 0.44],
        &[1.0, 1.0],
        1.0,
    );
    let low_better_side = score_hybrid_mono_consensus_candidate(
        "low_better_side",
        mono.clone(),
        &[0.50, 0.52],
        &[1.0, 1.0],
        0.25,
    );
    let low_worse_side = score_hybrid_mono_consensus_candidate(
        "low_worse_side",
        mono,
        &[0.42, 0.44],
        &[1.0, 1.0],
        0.25,
    );

    let high_gap = high_better_side.final_score - high_worse_side.final_score;
    let low_gap = low_better_side.final_score - low_worse_side.final_score;
    assert!(
        high_gap > low_gap,
        "expected lower side reliability to shrink the side tie-break gap: high {:.4} low {:.4}",
        high_gap,
        low_gap
    );
}

#[test]
fn fixed_trusted_window_metrics_match_direct_analysis_for_anchor_render() {
    fn tone_block(
        len: usize,
        left_hz: f32,
        right_hz: f32,
        left_gain: f32,
        right_gain: f32,
    ) -> Vec<f32> {
        (0..len)
            .flat_map(|idx| {
                let t = idx as f32 / SAMPLE_RATE as f32;
                let left = ((2.0 * std::f32::consts::PI * left_hz * t).sin()
                    + 0.35 * (2.0 * std::f32::consts::PI * left_hz * 1.5 * t).sin())
                    * left_gain;
                let right = ((2.0 * std::f32::consts::PI * right_hz * t).sin()
                    + 0.25 * (2.0 * std::f32::consts::PI * right_hz * 2.0 * t).sin())
                    * right_gain;
                [left, right]
            })
            .collect()
    }

    let intro = tone_block(22_050, 220.0, 330.0, 0.25, 0.18);
    let loop_phrase = tone_block(44_100, 440.0, 554.37, 0.55, 0.45);
    let outro = tone_block(22_050, 330.0, 247.0, 0.20, 0.16);

    let mut emu = intro;
    for _ in 0..9 {
        emu.extend_from_slice(&loop_phrase);
    }
    emu.extend_from_slice(&outro);

    let reference = emu.clone();

    let direct =
        analyze_trusted_reference_window(&emu, &reference).expect("expected direct analysis");
    let selection = select_trusted_reference_window(&emu, &reference)
        .expect("expected trusted window selection");
    let fixed = score_fixed_trusted_window(&emu, &reference, selection)
        .expect("expected fixed trusted score");

    assert_eq!(selection.trusted_len % 2, 0);
    assert!(selection.trusted_len > SAMPLE_RATE as usize);
    assert!((fixed.raw - direct.trusted_emu_raw).abs() < 1e-4);
    assert!((fixed.spectral - direct.trusted_emu_spectral).abs() < 1e-4);
    assert!((fixed.rms_ratio - direct.trusted_emu_rms).abs() < 1e-4);
    assert!((fixed.mid - direct.trusted_emu_mid).abs() < 1e-4);
    assert!((fixed.side - direct.trusted_emu_side).abs() < 1e-4);
}

#[test]
fn fixed_trusted_window_score_respects_selection_self_ceiling() {
    let stronger_selection = TrustedWindowSelection {
        env_corr: 0.5,
        local_env_corr: 0.72,
        trusted_score: 0.90,
        trusted_self_left: 0.90,
        trusted_self_mid: 0.90,
        trusted_self_side: 0.80,
        trusted_self_rms: 1.00,
        trusted_prior_start: 0,
        trusted_ref_start: 0,
        trusted_emu_start: 0,
        trusted_len: 1,
    };
    let weaker_selection = TrustedWindowSelection {
        env_corr: 0.4,
        local_env_corr: 0.60,
        trusted_score: 0.75,
        trusted_self_left: 0.75,
        trusted_self_mid: 0.75,
        trusted_self_side: 0.50,
        trusted_self_rms: 0.95,
        trusted_prior_start: 0,
        trusted_ref_start: 0,
        trusted_emu_start: 0,
        trusted_len: 1,
    };
    let stronger_metrics = FixedTrustedWindowMetrics {
        raw: 0.02,
        spectral: 0.81,
        rms_ratio: 0.95,
        mid: 0.81,
        side: 0.40,
    };
    let weaker_metrics = FixedTrustedWindowMetrics {
        raw: 0.01,
        spectral: 0.675,
        rms_ratio: 0.9025,
        mid: 0.675,
        side: 0.25,
    };

    let stronger_score = fixed_trusted_window_score(stronger_metrics, stronger_selection);
    let weaker_score = fixed_trusted_window_score(weaker_metrics, weaker_selection);
    assert!(
        (stronger_score - weaker_score).abs() < 0.02,
        "expected fixed-window normalization to treat equal relative fit similarly: {:.4} vs {:.4}",
        stronger_score,
        weaker_score
    );
}

#[test]
fn weighted_average_vectors_respects_weights() {
    let vectors = vec![vec![0.0, 1.0, 2.0], vec![2.0, 5.0, 8.0]];
    let weights = [1.0, 3.0];
    let averaged = weighted_average_vectors(&vectors, &weights);

    assert_eq!(averaged.len(), 3);
    assert!((averaged[0] - 1.5).abs() < 1e-6);
    assert!((averaged[1] - 4.0).abs() < 1e-6);
    assert!((averaged[2] - 6.5).abs() < 1e-6);
}

#[test]
fn largest_band_deltas_hz_reports_strongest_bins_first() {
    let a = [0.0, 0.0, 0.0];
    let b = [0.0, 0.5, -1.0];
    let bins = [10, 20, 30];
    let deltas = largest_band_deltas_hz(&a, &b, 100, 1_000, &bins, 2);

    assert_eq!(deltas.len(), 2);
    assert!((deltas[0].0 - 300.0).abs() < 1e-6);
    assert!(deltas[0].1 < 0.0);
    assert!((deltas[1].0 - 200.0).abs() < 1e-6);
    assert!(deltas[1].1 > 0.0);
}

#[test]
fn segmented_residual_profiles_are_zero_for_identical_signals() {
    let samples: Vec<f32> = (0..16_384)
        .map(|idx| {
            let t = idx as f32 / 44_100.0;
            (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.7
                + (2.0 * std::f32::consts::PI * 880.0 * t).sin() * 0.3
        })
        .collect();
    let bins = [8, 16, 24, 32, 40];
    let profiles = segmented_residual_profiles(&samples, &samples, 4_096, 1_024, 512, &bins);

    assert_eq!(profiles.len(), 4);
    for profile in profiles {
        assert!(profile.iter().all(|value| value.abs() < 1e-6));
    }
}

#[test]
fn residual_profile_stability_increases_when_segments_disagree() {
    let emu: Vec<f32> = (0..16_384)
        .map(|idx| {
            let t = idx as f32 / 44_100.0;
            (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.8
                + (2.0 * std::f32::consts::PI * 660.0 * t).sin() * 0.2
        })
        .collect();
    let mut reference = emu.clone();
    for (idx, sample) in reference.iter_mut().enumerate().skip(8_192) {
        let t = idx as f32 / 44_100.0;
        *sample += (2.0 * std::f32::consts::PI * 2_640.0 * t).sin() * 0.6;
    }
    let bins = [8, 16, 24, 32, 40, 48, 56, 64];
    let profiles = segmented_residual_profiles(&emu, &reference, 4_096, 1_024, 512, &bins);
    let stability = residual_profile_stability(&profiles);

    assert_eq!(profiles.len(), 4);
    assert!(
        stability > 0.05,
        "expected meaningful residual drift, got {stability:.4}"
    );
}

#[test]
fn partition_window_sections_covers_exact_multiple() {
    let sections = partition_window_sections(8, 2);

    assert_eq!(sections, vec![(0, 2), (2, 2), (4, 2), (6, 2)]);
}

#[test]
fn partition_window_sections_keeps_tail_segment() {
    let sections = partition_window_sections(9, 4);

    assert_eq!(sections, vec![(0, 4), (4, 4), (8, 1)]);
}

#[test]
fn best_reference_section_offset_prefers_zero_for_identical_sections() {
    let samples: Vec<f32> = (0..8_192)
        .map(|idx| {
            let t = idx as f32 / 44_100.0;
            (2.0 * std::f32::consts::PI * 440.0 * t).sin()
        })
        .collect();

    let (offset, corr) =
        best_reference_section_offset(&samples, &samples, 1_024, 1_024, 2_048, 256, 4)
            .expect("expected offset result");

    assert_eq!(offset, 0);
    assert!(corr > 0.999);
}

#[test]
fn best_reference_section_offset_recovers_small_positive_shift() {
    let emu: Vec<f32> = (0..10_240)
        .map(|idx| {
            let t = idx as f32 / 44_100.0;
            (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.7
                + (2.0 * std::f32::consts::PI * 660.0 * t).sin() * 0.3
        })
        .collect();
    let mut reference = vec![0.0; emu.len() + 512];
    reference[256..256 + emu.len()].copy_from_slice(&emu);

    let (offset, corr) =
        best_reference_section_offset(&emu, &reference, 2_048, 2_048, 2_048, 256, 4)
            .expect("expected shifted offset result");

    assert_eq!(offset, 256);
    assert!(corr > 0.95);
}

#[test]
fn repeated_consecutive_ratio_detects_flat_runs() {
    let samples = [0.0, 0.0, 1.0, 1.0, 1.0];
    let ratio = repeated_consecutive_ratio(&samples);

    assert!((ratio - 0.75).abs() < 1e-6);
}

#[test]
fn sample_jump_metrics_reports_large_transitions() {
    let samples = [0.0, 0.0, 0.5, 0.5, -0.2];
    let (mean_delta, max_delta, jumps) = sample_jump_metrics(&samples, 0.3);

    assert!((mean_delta - 0.3).abs() < 1e-6);
    assert!((max_delta - 0.7).abs() < 1e-6);
    assert_eq!(jumps, 2);
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_spectral_delta() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let ref_dir = Path::new(REFERENCE_DIR);
    let ref_path = ["sonic_ghz.flac", "sonic_ghz.wav"]
        .iter()
        .map(|f| ref_dir.join(f))
        .find(|p| p.exists())
        .expect("expected GHZ reference audio");
    let (_ref_rate, ref_samples) = load_reference(&ref_path).expect("failed to load reference");

    let record_frames = ((ref_samples.len() as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let emu_samples = generate_ghz_audio(&rom, record_frames);

    let emu_left: Vec<f32> = emu_samples.iter().step_by(2).copied().collect();
    let ref_left: Vec<f32> = ref_samples.iter().step_by(2).copied().collect();
    let env_window = 2048;
    let emu_env = rms_envelope(&emu_left, env_window);
    let ref_env = rms_envelope(&ref_left, env_window);
    let max_lag_windows = emu_env.len().min(ref_env.len()).saturating_sub(64);
    let (_env_corr, env_lag) = best_lagged_correlation(&emu_env, &ref_env, max_lag_windows);
    let local_env_window = (SAMPLE_RATE as usize * 8 / env_window).max(64);
    let local_match = best_windowed_match_at_lag(&emu_env, &ref_env, env_lag, local_env_window)
        .expect("expected a usable local GHZ envelope match");
    let local_emu_start = local_match.a_start * env_window;
    let local_ref_start = local_match.b_start * env_window;
    let local_raw_len = (local_match.len * env_window)
        .min(emu_left.len().saturating_sub(local_emu_start))
        .min(ref_left.len().saturating_sub(local_ref_start));

    let frame_len = 2048;
    let hop_len = 1024;
    let bins = log_frequency_bins(frame_len, SAMPLE_RATE, 80.0, 12_000.0, 24);
    let emu_spec = average_log_spectrum(
        &emu_left[local_emu_start..local_emu_start + local_raw_len],
        frame_len,
        hop_len,
        &bins,
    );
    let ref_spec = average_log_spectrum(
        &ref_left[local_ref_start..local_ref_start + local_raw_len],
        frame_len,
        hop_len,
        &bins,
    );

    eprintln!(
        "=== GHZ spectral delta === local_env={:.4} bins={} frame_len={} hop_len={}",
        local_match.corr,
        bins.len(),
        frame_len,
        hop_len
    );
    for (idx, (&bin, (&emu, &reference))) in bins
        .iter()
        .zip(emu_spec.iter().zip(ref_spec.iter()))
        .enumerate()
    {
        let delta_db = (reference - emu) * 4.342_944_8;
        eprintln!(
            "{:2}: {:7.1} Hz  emu={:+.4} ref={:+.4} delta={:+.2} dB",
            idx,
            bin_center_hz(frame_len, SAMPLE_RATE, bin),
            emu,
            reference,
            delta_db
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_mid_side_delta() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let ref_dir = Path::new(REFERENCE_DIR);
    let ref_path = ["sonic_ghz.flac", "sonic_ghz.wav"]
        .iter()
        .map(|f| ref_dir.join(f))
        .find(|p| p.exists())
        .expect("expected GHZ reference audio");
    let (_ref_rate, ref_samples) = load_reference(&ref_path).expect("failed to load reference");

    let record_frames = ((ref_samples.len() as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let emu_samples = generate_ghz_audio(&rom, record_frames);

    let emu_left: Vec<f32> = emu_samples.iter().step_by(2).copied().collect();
    let ref_left: Vec<f32> = ref_samples.iter().step_by(2).copied().collect();
    let env_window = 2048;
    let emu_env = rms_envelope(&emu_left, env_window);
    let ref_env = rms_envelope(&ref_left, env_window);
    let max_lag_windows = emu_env.len().min(ref_env.len()).saturating_sub(64);
    let (_env_corr, env_lag) = best_lagged_correlation(&emu_env, &ref_env, max_lag_windows);
    let local_env_window = (SAMPLE_RATE as usize * 8 / env_window).max(64);
    let local_match = best_windowed_match_at_lag(&emu_env, &ref_env, env_lag, local_env_window)
        .expect("expected a usable local GHZ envelope match");
    let local_emu_start = local_match.a_start * env_window;
    let local_ref_start = local_match.b_start * env_window;
    let local_raw_len = (local_match.len * env_window)
        .min(emu_left.len().saturating_sub(local_emu_start))
        .min(ref_left.len().saturating_sub(local_ref_start));

    let emu_stereo_local = stereo_window(&emu_samples, local_emu_start, local_raw_len);
    let ref_stereo_local = stereo_window(&ref_samples, local_ref_start, local_raw_len);
    let (emu_mid, emu_side) = stereo_mid_side(emu_stereo_local);
    let (ref_mid, ref_side) = stereo_mid_side(ref_stereo_local);

    let frame_len = 2048;
    let hop_len = 1024;
    let bins = log_frequency_bins(frame_len, SAMPLE_RATE, 80.0, 12_000.0, 24);
    let emu_mid_spec = average_log_spectrum(&emu_mid, frame_len, hop_len, &bins);
    let ref_mid_spec = average_log_spectrum(&ref_mid, frame_len, hop_len, &bins);
    let emu_side_spec = average_log_spectrum(&emu_side, frame_len, hop_len, &bins);
    let ref_side_spec = average_log_spectrum(&ref_side, frame_len, hop_len, &bins);
    let mid_similarity = cosine_similarity(&emu_mid_spec, &ref_mid_spec);
    let side_similarity = cosine_similarity(&emu_side_spec, &ref_side_spec);

    eprintln!(
        "=== GHZ mid/side delta === local_env={:.4} mid_similarity={:.4} side_similarity={:.4}",
        local_match.corr, mid_similarity, side_similarity
    );
    eprintln!("-- Mid spectrum delta --");
    for (idx, (&bin, (&emu, &reference))) in bins
        .iter()
        .zip(emu_mid_spec.iter().zip(ref_mid_spec.iter()))
        .enumerate()
    {
        let delta_db = (reference - emu) * 4.342_944_8;
        eprintln!(
            "{:2}: {:7.1} Hz  emu={:+.4} ref={:+.4} delta={:+.2} dB",
            idx,
            bin_center_hz(frame_len, SAMPLE_RATE, bin),
            emu,
            reference,
            delta_db
        );
    }
    eprintln!("-- Side spectrum delta --");
    for (idx, (&bin, (&emu, &reference))) in bins
        .iter()
        .zip(emu_side_spec.iter().zip(ref_side_spec.iter()))
        .enumerate()
    {
        let delta_db = (reference - emu) * 4.342_944_8;
        eprintln!(
            "{:2}: {:7.1} Hz  emu={:+.4} ref={:+.4} delta={:+.2} dB",
            idx,
            bin_center_hz(frame_len, SAMPLE_RATE, bin),
            emu,
            reference,
            delta_db
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_left_right_delta() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let ref_dir = Path::new(REFERENCE_DIR);
    let ref_path = ["sonic_ghz.flac", "sonic_ghz.wav"]
        .iter()
        .map(|f| ref_dir.join(f))
        .find(|p| p.exists())
        .expect("expected GHZ reference audio");
    let (_ref_rate, ref_samples) = load_reference(&ref_path).expect("failed to load reference");

    let record_frames = ((ref_samples.len() as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let emu_samples = generate_ghz_audio(&rom, record_frames);

    let emu_left: Vec<f32> = emu_samples.iter().step_by(2).copied().collect();
    let ref_left: Vec<f32> = ref_samples.iter().step_by(2).copied().collect();
    let env_window = 2048;
    let emu_env = rms_envelope(&emu_left, env_window);
    let ref_env = rms_envelope(&ref_left, env_window);
    let max_lag_windows = emu_env.len().min(ref_env.len()).saturating_sub(64);
    let (_env_corr, env_lag) = best_lagged_correlation(&emu_env, &ref_env, max_lag_windows);
    let local_env_window = (SAMPLE_RATE as usize * 8 / env_window).max(64);
    let local_match = best_windowed_match_at_lag(&emu_env, &ref_env, env_lag, local_env_window)
        .expect("expected a usable local GHZ envelope match");
    let local_emu_start = local_match.a_start * env_window;
    let local_ref_start = local_match.b_start * env_window;
    let local_raw_len = (local_match.len * env_window)
        .min(emu_left.len().saturating_sub(local_emu_start))
        .min(ref_left.len().saturating_sub(local_ref_start));

    let emu_stereo_local = stereo_window(&emu_samples, local_emu_start, local_raw_len);
    let ref_stereo_local = stereo_window(&ref_samples, local_ref_start, local_raw_len);
    let (emu_left_local, emu_right_local) = stereo_left_right(emu_stereo_local);
    let (ref_left_local, ref_right_local) = stereo_left_right(ref_stereo_local);

    let frame_len = 2048;
    let hop_len = 1024;
    let bins = log_frequency_bins(frame_len, SAMPLE_RATE, 80.0, 12_000.0, 24);
    let emu_left_spec = average_log_spectrum(&emu_left_local, frame_len, hop_len, &bins);
    let ref_left_spec = average_log_spectrum(&ref_left_local, frame_len, hop_len, &bins);
    let emu_right_spec = average_log_spectrum(&emu_right_local, frame_len, hop_len, &bins);
    let ref_right_spec = average_log_spectrum(&ref_right_local, frame_len, hop_len, &bins);
    let left_similarity = cosine_similarity(&emu_left_spec, &ref_left_spec);
    let right_similarity = cosine_similarity(&emu_right_spec, &ref_right_spec);

    eprintln!(
        "=== GHZ left/right delta === local_env={:.4} left_similarity={:.4} right_similarity={:.4}",
        local_match.corr, left_similarity, right_similarity
    );
    eprintln!("-- Left spectrum delta --");
    for (idx, (&bin, (&emu, &reference))) in bins
        .iter()
        .zip(emu_left_spec.iter().zip(ref_left_spec.iter()))
        .enumerate()
    {
        let delta_db = (reference - emu) * 4.342_944_8;
        eprintln!(
            "{:2}: {:7.1} Hz  emu={:+.4} ref={:+.4} delta={:+.2} dB",
            idx,
            bin_center_hz(frame_len, SAMPLE_RATE, bin),
            emu,
            reference,
            delta_db
        );
    }
    eprintln!("-- Right spectrum delta --");
    for (idx, (&bin, (&emu, &reference))) in bins
        .iter()
        .zip(emu_right_spec.iter().zip(ref_right_spec.iter()))
        .enumerate()
    {
        let delta_db = (reference - emu) * 4.342_944_8;
        eprintln!(
            "{:2}: {:7.1} Hz  emu={:+.4} ref={:+.4} delta={:+.2} dB",
            idx,
            bin_center_hz(frame_len, SAMPLE_RATE, bin),
            emu,
            reference,
            delta_db
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_phase_delta() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let ref_dir = Path::new(REFERENCE_DIR);
    let ref_path = ["sonic_ghz.flac", "sonic_ghz.wav"]
        .iter()
        .map(|f| ref_dir.join(f))
        .find(|p| p.exists())
        .expect("expected GHZ reference audio");
    let (_ref_rate, ref_samples) = load_reference(&ref_path).expect("failed to load reference");

    let record_frames = ((ref_samples.len() as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let emu_samples = generate_ghz_audio(&rom, record_frames);

    let emu_left: Vec<f32> = emu_samples.iter().step_by(2).copied().collect();
    let ref_left: Vec<f32> = ref_samples.iter().step_by(2).copied().collect();
    let env_window = 2048;
    let emu_env = rms_envelope(&emu_left, env_window);
    let ref_env = rms_envelope(&ref_left, env_window);
    let max_lag_windows = emu_env.len().min(ref_env.len()).saturating_sub(64);
    let (_env_corr, env_lag) = best_lagged_correlation(&emu_env, &ref_env, max_lag_windows);
    let local_env_window = (SAMPLE_RATE as usize * 8 / env_window).max(64);
    let local_match = best_windowed_match_at_lag(&emu_env, &ref_env, env_lag, local_env_window)
        .expect("expected a usable local GHZ envelope match");
    let local_emu_start = local_match.a_start * env_window;
    let local_ref_start = local_match.b_start * env_window;
    let local_raw_len = (local_match.len * env_window)
        .min(emu_left.len().saturating_sub(local_emu_start))
        .min(ref_left.len().saturating_sub(local_ref_start));

    let emu_stereo_local = stereo_window(&emu_samples, local_emu_start, local_raw_len);
    let ref_stereo_local = stereo_window(&ref_samples, local_ref_start, local_raw_len);
    let (emu_left_local, emu_right_local) = stereo_left_right(emu_stereo_local);
    let (ref_left_local, ref_right_local) = stereo_left_right(ref_stereo_local);
    let (emu_mid, emu_side) = stereo_mid_side(emu_stereo_local);
    let (ref_mid, ref_side) = stereo_mid_side(ref_stereo_local);

    let frame_len = 2048;
    let hop_len = 1024;
    let bins = log_frequency_bins(frame_len, SAMPLE_RATE, 80.0, 12_000.0, 24);
    let left_phase =
        average_phase_delta(&emu_left_local, &ref_left_local, frame_len, hop_len, &bins);
    let right_phase = average_phase_delta(
        &emu_right_local,
        &ref_right_local,
        frame_len,
        hop_len,
        &bins,
    );
    let mid_phase = average_phase_delta(&emu_mid, &ref_mid, frame_len, hop_len, &bins);
    let side_phase = average_phase_delta(&emu_side, &ref_side, frame_len, hop_len, &bins);

    eprintln!("=== GHZ phase delta === local_env={:.4}", local_match.corr);
    eprintln!("bin     hz    left_deg coh   right_deg coh   mid_deg coh    side_deg coh");
    for idx in 0..bins.len() {
        eprintln!(
            "{:2} {:7.1} {:8.2} {:5.3} {:10.2} {:5.3} {:10.2} {:5.3} {:11.2} {:5.3}",
            idx,
            bin_center_hz(frame_len, SAMPLE_RATE, bins[idx]),
            left_phase[idx].0,
            left_phase[idx].1,
            right_phase[idx].0,
            right_phase[idx].1,
            mid_phase[idx].0,
            mid_phase[idx].1,
            side_phase[idx].0,
            side_phase[idx].1,
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_chip_balance() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let ref_dir = Path::new(REFERENCE_DIR);
    let ref_path = ["sonic_ghz.flac", "sonic_ghz.wav"]
        .iter()
        .map(|f| ref_dir.join(f))
        .find(|p| p.exists())
        .expect("expected GHZ reference audio");
    let (_ref_rate, ref_samples) = load_reference(&ref_path).expect("failed to load reference");

    let record_frames = ((ref_samples.len() as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);

    let ref_left: Vec<f32> = ref_samples.iter().step_by(2).copied().collect();
    let env_window = 2048usize;
    let ref_env = rms_envelope(&ref_left, env_window);
    let spectral_bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 12_000.0, 24);

    let candidates = [
        ("current_default", AudioOutputConfig::default()),
        ("ym_only", AudioOutputConfig::default().with_psg_gain(0.0)),
        ("psg_only", AudioOutputConfig::default().with_ym_gain(0.0)),
        ("psg_half", AudioOutputConfig::default().with_psg_gain(0.4)),
        (
            "psg_plus25",
            AudioOutputConfig::default().with_psg_gain(1.0),
        ),
    ];

    eprintln!("=== GHZ chip balance ===");
    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        let emu_left: Vec<f32> = rendered.iter().step_by(2).copied().collect();
        let emu_env = rms_envelope(&emu_left, env_window);
        let max_lag_windows = emu_env.len().min(ref_env.len()).saturating_sub(64);
        let (env_corr, env_lag) = best_lagged_correlation(&emu_env, &ref_env, max_lag_windows);
        let local_env_window = (SAMPLE_RATE as usize * 8 / env_window).max(64);
        let local_match =
            best_windowed_match_at_lag(&emu_env, &ref_env, env_lag, local_env_window).unwrap();
        let local_emu_start = local_match.a_start * env_window;
        let local_ref_start = local_match.b_start * env_window;
        let local_raw_len = (local_match.len * env_window)
            .min(emu_left.len().saturating_sub(local_emu_start))
            .min(ref_left.len().saturating_sub(local_ref_start));
        let spectral = spectral_similarity(
            &emu_left[local_emu_start..local_emu_start + local_raw_len],
            &ref_left[local_ref_start..local_ref_start + local_raw_len],
            2048,
            1024,
            &spectral_bins,
        );
        let rms_ratio = rms(&emu_left[local_emu_start..local_emu_start + local_raw_len])
            / rms(&ref_left[local_ref_start..local_ref_start + local_raw_len]).max(1e-9);

        eprintln!(
            "{name:>16}: env={env_corr:.4} local_env={:.4} spectral={spectral:.4} local_rms={rms_ratio:.4} lag={env_lag:+}",
            local_match.corr
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_source_fit() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let ref_dir = Path::new(REFERENCE_DIR);
    let ref_path = ["sonic_ghz.flac", "sonic_ghz.wav"]
        .iter()
        .map(|f| ref_dir.join(f))
        .find(|p| p.exists())
        .expect("expected GHZ reference audio");
    let (_ref_rate, ref_samples) = load_reference(&ref_path).expect("failed to load reference");

    let record_frames = ((ref_samples.len() as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let config = AudioOutputConfig::default();

    let render_with = |config: AudioOutputConfig| {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        rendered_full[capture_start..].to_vec()
    };

    let default_rendered = render_with(config);
    let ym_only_rendered = render_with(config.with_psg_gain(0.0));
    let psg_only_rendered = render_with(config.with_ym_gain(0.0));

    let default_left: Vec<f32> = default_rendered.iter().step_by(2).copied().collect();
    let ref_left: Vec<f32> = ref_samples.iter().step_by(2).copied().collect();
    let env_window = 2048usize;
    let default_env = rms_envelope(&default_left, env_window);
    let ref_env = rms_envelope(&ref_left, env_window);
    let max_lag_windows = default_env.len().min(ref_env.len()).saturating_sub(64);
    let (_env_corr, env_lag) = best_lagged_correlation(&default_env, &ref_env, max_lag_windows);
    let local_env_window = (SAMPLE_RATE as usize * 8 / env_window).max(64);
    let local_match =
        best_windowed_match_at_lag(&default_env, &ref_env, env_lag, local_env_window).unwrap();
    let local_emu_start = local_match.a_start * env_window;
    let local_ref_start = local_match.b_start * env_window;
    let local_raw_len = (local_match.len * env_window)
        .min(default_left.len().saturating_sub(local_emu_start))
        .min(ref_left.len().saturating_sub(local_ref_start));

    let default_local = stereo_window(&default_rendered, local_emu_start, local_raw_len);
    let ym_local = stereo_window(&ym_only_rendered, local_emu_start, local_raw_len);
    let psg_local = stereo_window(&psg_only_rendered, local_emu_start, local_raw_len);
    let ref_local = stereo_window(&ref_samples, local_ref_start, local_raw_len);
    let spectral_bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 12_000.0, 24);

    let (default_self_ym_scale, default_self_psg_scale) =
        fit_two_source_mix(ym_local, psg_local, default_local)
            .expect("expected default self-fit to be solvable");
    let (stereo_ym_scale, stereo_psg_scale) =
        fit_two_source_mix(ym_local, psg_local, ref_local).expect("expected stereo source fit");
    let fitted_stereo = mix_two_sources(ym_local, psg_local, stereo_ym_scale, stereo_psg_scale);
    let fitted_left: Vec<f32> = fitted_stereo.iter().step_by(2).copied().collect();
    let ref_left_local: Vec<f32> = ref_local.iter().step_by(2).copied().collect();
    let fitted_spectral =
        spectral_similarity(&fitted_left, &ref_left_local, 2048, 1024, &spectral_bins);
    let fitted_corr = cross_correlation(&fitted_left, &ref_left_local);
    let fitted_rms_ratio = rms(&fitted_left) / rms(&ref_left_local).max(1e-9);

    let (ym_left, ym_right) = stereo_left_right(ym_local);
    let (psg_left, psg_right) = stereo_left_right(psg_local);
    let (ref_left_local_lr, ref_right_local_lr) = stereo_left_right(ref_local);
    let (left_ym_scale, left_psg_scale) =
        fit_two_source_mix(&ym_left, &psg_left, &ref_left_local_lr).expect("expected left fit");
    let (right_ym_scale, right_psg_scale) =
        fit_two_source_mix(&ym_right, &psg_right, &ref_right_local_lr).expect("expected right fit");

    let default_local_left: Vec<f32> = default_local.iter().step_by(2).copied().collect();
    let default_local_spectral = spectral_similarity(
        &default_local_left,
        &ref_left_local,
        2048,
        1024,
        &spectral_bins,
    );
    let default_local_corr = cross_correlation(&default_local_left, &ref_left_local);
    let default_local_rms_ratio = rms(&default_local_left) / rms(&ref_left_local).max(1e-9);
    let ym_power = average_power_spectrum(&ym_left, 2048, 1024, &spectral_bins);
    let psg_power = average_power_spectrum(&psg_left, 2048, 1024, &spectral_bins);
    let default_power = average_power_spectrum(&default_local_left, 2048, 1024, &spectral_bins);
    let ref_power = average_power_spectrum(&ref_left_local, 2048, 1024, &spectral_bins);
    let (default_power_ym_scale, default_power_psg_scale) =
        fit_two_source_mix(&ym_power, &psg_power, &default_power)
            .expect("expected default power fit");
    let (ref_power_ym_scale, ref_power_psg_scale) =
        fit_two_source_mix(&ym_power, &psg_power, &ref_power)
            .expect("expected reference power fit");
    let fitted_ref_power = mix_two_sources(
        &ym_power,
        &psg_power,
        ref_power_ym_scale,
        ref_power_psg_scale,
    );
    let ref_power_cos = cosine_similarity(&fitted_ref_power, &ref_power);

    eprintln!("=== GHZ source fit ===");
    eprintln!(
        "default local: corr={default_local_corr:.4} spectral={default_local_spectral:.4} rms={default_local_rms_ratio:.4}"
    );
    eprintln!(
        "raw-sample fit: default_self=({default_self_ym_scale:.4},{default_self_psg_scale:.4}) ref=({stereo_ym_scale:.4},{stereo_psg_scale:.4}) corr={fitted_corr:.4} spectral={fitted_spectral:.4} rms={fitted_rms_ratio:.4}"
    );
    eprintln!(
        "fitted left/right scales: left=({left_ym_scale:.4},{left_psg_scale:.4}) right=({right_ym_scale:.4},{right_psg_scale:.4})"
    );
    eprintln!(
        "power-spectrum fit: default_self=({default_power_ym_scale:.4},{default_power_psg_scale:.4}) ref=({ref_power_ym_scale:.4},{ref_power_psg_scale:.4}) cos={ref_power_cos:.4}"
    );
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_fixed_trusted_window_chip_isolation() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);

    let candidates = [
        ("current_default", AudioOutputConfig::default()),
        ("ym_only", AudioOutputConfig::default().with_psg_gain(0.0)),
        ("psg_only", AudioOutputConfig::default().with_ym_gain(0.0)),
        ("psg_0_70", AudioOutputConfig::default().with_psg_gain(0.70)),
        ("psg_0_90", AudioOutputConfig::default().with_psg_gain(0.90)),
        ("psg_1_00", AudioOutputConfig::default().with_psg_gain(1.00)),
    ];

    let mut rendered_candidates = Vec::new();
    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        rendered_candidates.push((name, rendered_full[capture_start..].to_vec()));
    }

    let default_rendered = rendered_candidates
        .iter()
        .find(|(name, _)| *name == "current_default")
        .map(|(_, samples)| samples.as_slice())
        .expect("expected current_default render");
    let candidate_names: Vec<_> = rendered_candidates.iter().map(|(name, _)| *name).collect();
    let mut weighted_scores = vec![0.0f32; rendered_candidates.len()];
    let mut total_weight = 0.0f32;

    eprintln!("=== GHZ fixed trusted-window chip isolation ===");
    for (ref_path, ref_samples) in &loaded_refs {
        let ref_name = ref_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let Some(selection) = select_trusted_reference_window(default_rendered, ref_samples) else {
            eprintln!("{ref_name:>20}: unable to select trusted anchor window");
            continue;
        };
        let weight = trusted_window_reliability_weight(selection);
        total_weight += weight;
        eprintln!(
            "-- {ref_name} -- weight={weight:.4} trusted_start={:.2}s len={:.2}s self=({:.4}/{:.4}/{:.4}/{:.4})",
            selection.trusted_ref_start as f32 / SAMPLE_RATE as f32,
            selection.trusted_len as f32 / SAMPLE_RATE as f32,
            selection.trusted_self_left,
            selection.trusted_self_mid,
            selection.trusted_self_side,
            selection.trusted_self_rms,
        );

        let default_metrics = score_fixed_trusted_window(default_rendered, ref_samples, selection)
            .expect("default render should score on its own selection");
        let default_score = fixed_trusted_window_score(default_metrics, selection);

        for (idx, (name, rendered)) in rendered_candidates.iter().enumerate() {
            let metrics = score_fixed_trusted_window(rendered, ref_samples, selection)
                .expect("candidate render should score on trusted window");
            let score = fixed_trusted_window_score(metrics, selection);
            weighted_scores[idx] += score * weight;
            eprintln!(
                "{name:>16}: score={score:.4} delta={:+.4} spectral={:.4} rms={:.4} mid={:.4} side={:.4} raw={:.4}",
                score - default_score,
                metrics.spectral,
                metrics.rms_ratio,
                metrics.mid,
                metrics.side,
                metrics.raw,
            );
        }
    }

    if total_weight > 0.0 {
        eprintln!("=== weighted chip-isolation summary ===");
        for (name, score) in candidate_names.iter().zip(weighted_scores) {
            eprintln!("{name:>16}: weighted_score={:.4}", score / total_weight);
        }
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_fixed_trusted_window_ym_reference_isolation() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);

    let current_default = AudioOutputConfig::default();
    let ym_only_config = current_default.with_psg_gain(0.0);
    let mut default_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let default_rendered_full = default_renderer.render_timed_writes(
        &trace.ym_writes,
        &trace.psg_writes,
        0,
        trace.end_tick,
    );
    let mut genesoxide_ym_renderer = CoreAudioRenderer::with_audio_output_config(ym_only_config);
    let genesoxide_ym_full = genesoxide_ym_renderer.render_timed_writes(
        &trace.ym_writes,
        &trace.psg_writes,
        0,
        trace.end_tick,
    );

    let ym_vgm = vgm_from_timed_ym2612_writes(&trace.ym_writes, 0, trace.end_tick);
    let mut ymfm_renderer = Ymfm2612Renderer::with_clock(7_670_453);
    let ymfm_raw_full = ymfm_renderer.render(&ym_vgm);
    let mut ymfm_shaper = CoreAudioRenderer::with_audio_output_config(ym_only_config);
    let ymfm_shaped_full = ymfm_shaper.render_external_ym_stream(&ymfm_raw_full);

    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(default_rendered_full.len())
        .min(genesoxide_ym_full.len())
        .min(ymfm_shaped_full.len());
    let default_rendered = &default_rendered_full[capture_start..];
    let genesoxide_ym = &genesoxide_ym_full[capture_start..];
    let ymfm_ym = &ymfm_shaped_full[capture_start..];

    let mut weighted_default = 0.0f32;
    let mut weighted_genesoxide_ym = 0.0f32;
    let mut weighted_ymfm_ym = 0.0f32;
    let mut total_weight = 0.0f32;

    eprintln!("=== GHZ fixed trusted-window YM reference isolation ===");
    for (ref_path, ref_samples) in &loaded_refs {
        let ref_name = ref_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let Some(selection) = select_trusted_reference_window(default_rendered, ref_samples) else {
            eprintln!("{ref_name:>20}: unable to select trusted anchor window");
            continue;
        };
        let weight = trusted_window_reliability_weight(selection);
        total_weight += weight;
        let default_metrics = score_fixed_trusted_window(default_rendered, ref_samples, selection)
            .expect("default render should score on its own selection");
        let genesoxide_metrics = score_fixed_trusted_window(genesoxide_ym, ref_samples, selection)
            .expect("genesoxide YM-only render should score on trusted window");
        let ymfm_metrics = score_fixed_trusted_window(ymfm_ym, ref_samples, selection)
            .expect("ymfm YM-only render should score on trusted window");
        let default_score = fixed_trusted_window_score(default_metrics, selection);
        let genesoxide_score = fixed_trusted_window_score(genesoxide_metrics, selection);
        let ymfm_score = fixed_trusted_window_score(ymfm_metrics, selection);
        weighted_default += default_score * weight;
        weighted_genesoxide_ym += genesoxide_score * weight;
        weighted_ymfm_ym += ymfm_score * weight;

        eprintln!(
            "-- {ref_name} -- weight={weight:.4} trusted_start={:.2}s len={:.2}s",
            selection.trusted_ref_start as f32 / SAMPLE_RATE as f32,
            selection.trusted_len as f32 / SAMPLE_RATE as f32,
        );
        eprintln!(
            "{:>16}: score={:.4} spectral={:.4} rms={:.4} mid={:.4} side={:.4}",
            "current_default",
            default_score,
            default_metrics.spectral,
            default_metrics.rms_ratio,
            default_metrics.mid,
            default_metrics.side,
        );
        eprintln!(
            "{:>16}: score={:.4} delta={:+.4} spectral={:.4} rms={:.4} mid={:.4} side={:.4}",
            "genesoxide_ym",
            genesoxide_score,
            genesoxide_score - default_score,
            genesoxide_metrics.spectral,
            genesoxide_metrics.rms_ratio,
            genesoxide_metrics.mid,
            genesoxide_metrics.side,
        );
        eprintln!(
            "{:>16}: score={:.4} delta_vs_default={:+.4} delta_vs_genesoxide={:+.4} spectral={:.4} rms={:.4} mid={:.4} side={:.4}",
            "ymfm_ym",
            ymfm_score,
            ymfm_score - default_score,
            ymfm_score - genesoxide_score,
            ymfm_metrics.spectral,
            ymfm_metrics.rms_ratio,
            ymfm_metrics.mid,
            ymfm_metrics.side,
        );
    }

    if total_weight > 0.0 {
        eprintln!("=== weighted YM reference summary ===");
        eprintln!(
            "{:>16}: weighted_score={:.4}",
            "current_default",
            weighted_default / total_weight
        );
        eprintln!(
            "{:>16}: weighted_score={:.4}",
            "genesoxide_ym",
            weighted_genesoxide_ym / total_weight
        );
        eprintln!(
            "{:>16}: weighted_score={:.4}",
            "ymfm_ym",
            weighted_ymfm_ym / total_weight
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_reference_normalization_consensus() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let mut renderer = CoreAudioRenderer::with_audio_output_config(AudioOutputConfig::default());
    let rendered_full =
        renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(rendered_full.len());
    let rendered = &rendered_full[capture_start..];

    let frame_len = 2048usize;
    let hop_len = 1024usize;
    let spectral_bins = log_frequency_bins(frame_len, SAMPLE_RATE, 80.0, 12_000.0, 24);

    let mut ref_names = Vec::new();
    let mut weights = Vec::new();
    let mut ref_left_specs = Vec::new();
    let mut ref_mid_specs = Vec::new();
    let mut ref_side_specs = Vec::new();
    let mut emu_left_specs = Vec::new();
    let mut emu_mid_specs = Vec::new();
    let mut emu_side_specs = Vec::new();

    for (ref_path, ref_samples) in &loaded_refs {
        let Some(selection) = select_trusted_reference_window(rendered, ref_samples) else {
            continue;
        };
        let weight = trusted_window_reliability_weight(selection);
        let ref_window = stereo_window(
            ref_samples,
            selection.trusted_ref_start,
            selection.trusted_len,
        );
        let emu_window =
            stereo_window(rendered, selection.trusted_emu_start, selection.trusted_len);
        let (ref_left, ref_mid, ref_side) =
            stereo_log_spectra(ref_window, frame_len, hop_len, &spectral_bins);
        let (emu_left, emu_mid, emu_side) =
            stereo_log_spectra(emu_window, frame_len, hop_len, &spectral_bins);

        ref_names.push(
            ref_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_owned(),
        );
        weights.push(weight);
        ref_left_specs.push(ref_left);
        ref_mid_specs.push(ref_mid);
        ref_side_specs.push(ref_side);
        emu_left_specs.push(emu_left);
        emu_mid_specs.push(emu_mid);
        emu_side_specs.push(emu_side);
    }

    if ref_names.is_empty() {
        eprintln!("No trusted GHZ reference windows available, skipping");
        return;
    }

    let consensus_left = weighted_average_vectors(&ref_left_specs, &weights);
    let consensus_mid = weighted_average_vectors(&ref_mid_specs, &weights);
    let consensus_side = weighted_average_vectors(&ref_side_specs, &weights);
    let emu_consensus_left = weighted_average_vectors(&emu_left_specs, &weights);
    let emu_consensus_mid = weighted_average_vectors(&emu_mid_specs, &weights);
    let emu_consensus_side = weighted_average_vectors(&emu_side_specs, &weights);
    let ln_to_db = 10.0 / std::f32::consts::LN_10;

    eprintln!("=== GHZ reference normalization consensus ===");
    for idx in 0..ref_names.len() {
        let left_cos = cosine_similarity(&ref_left_specs[idx], &consensus_left);
        let mid_cos = cosine_similarity(&ref_mid_specs[idx], &consensus_mid);
        let side_cos = cosine_similarity(&ref_side_specs[idx], &consensus_side);
        let left_mad_db = mean_abs_delta(&ref_left_specs[idx], &consensus_left) * ln_to_db;
        let mid_mad_db = mean_abs_delta(&ref_mid_specs[idx], &consensus_mid) * ln_to_db;
        let side_mad_db = mean_abs_delta(&ref_side_specs[idx], &consensus_side) * ln_to_db;
        eprintln!(
            "{:>20}: weight={:.4} left=({left_cos:.4}/{left_mad_db:.2}dB) mid=({mid_cos:.4}/{mid_mad_db:.2}dB) side=({side_cos:.4}/{side_mad_db:.2}dB)",
            ref_names[idx], weights[idx],
        );
    }

    let emu_left_cos = cosine_similarity(&emu_consensus_left, &consensus_left);
    let emu_mid_cos = cosine_similarity(&emu_consensus_mid, &consensus_mid);
    let emu_side_cos = cosine_similarity(&emu_consensus_side, &consensus_side);
    let emu_left_mad_db = mean_abs_delta(&emu_consensus_left, &consensus_left) * ln_to_db;
    let emu_mid_mad_db = mean_abs_delta(&emu_consensus_mid, &consensus_mid) * ln_to_db;
    let emu_side_mad_db = mean_abs_delta(&emu_consensus_side, &consensus_side) * ln_to_db;
    eprintln!(
        "{:>20}: left=({emu_left_cos:.4}/{emu_left_mad_db:.2}dB) mid=({emu_mid_cos:.4}/{emu_mid_mad_db:.2}dB) side=({emu_side_cos:.4}/{emu_side_mad_db:.2}dB)",
        "current_default"
    );

    if ref_names.len() >= 2 {
        let left_deltas = largest_band_deltas_hz(
            &ref_left_specs[0],
            &ref_left_specs[1],
            frame_len,
            SAMPLE_RATE,
            &spectral_bins,
            5,
        );
        let mid_deltas = largest_band_deltas_hz(
            &ref_mid_specs[0],
            &ref_mid_specs[1],
            frame_len,
            SAMPLE_RATE,
            &spectral_bins,
            5,
        );
        let side_deltas = largest_band_deltas_hz(
            &ref_side_specs[0],
            &ref_side_specs[1],
            frame_len,
            SAMPLE_RATE,
            &spectral_bins,
            5,
        );
        let format_deltas = |deltas: &[(f32, f32)]| -> String {
            deltas
                .iter()
                .map(|(hz, db)| format!("{hz:.0}Hz {db:+.2}dB"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        eprintln!(
            "pairwise capture deltas {} -> {}:",
            ref_names[0], ref_names[1]
        );
        eprintln!("  left: {}", format_deltas(&left_deltas));
        eprintln!("  mid:  {}", format_deltas(&mid_deltas));
        eprintln!("  side: {}", format_deltas(&side_deltas));
    }

    let emu_left_deltas = largest_band_deltas_hz(
        &emu_consensus_left,
        &consensus_left,
        frame_len,
        SAMPLE_RATE,
        &spectral_bins,
        5,
    );
    let emu_mid_deltas = largest_band_deltas_hz(
        &emu_consensus_mid,
        &consensus_mid,
        frame_len,
        SAMPLE_RATE,
        &spectral_bins,
        5,
    );
    let emu_side_deltas = largest_band_deltas_hz(
        &emu_consensus_side,
        &consensus_side,
        frame_len,
        SAMPLE_RATE,
        &spectral_bins,
        5,
    );
    let format_deltas = |deltas: &[(f32, f32)]| -> String {
        deltas
            .iter()
            .map(|(hz, db)| format!("{hz:.0}Hz {db:+.2}dB"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    eprintln!("current_default -> reference consensus:");
    eprintln!("  left: {}", format_deltas(&emu_left_deltas));
    eprintln!("  mid:  {}", format_deltas(&emu_mid_deltas));
    eprintln!("  side: {}", format_deltas(&emu_side_deltas));
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_reference_normalized_profile_candidates() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let candidates = [
        ("current_default", current_default),
        (
            "eq4_190_plus1p5",
            current_default.with_post_eq_4(AudioEqStage::peaking(190.0, 0.90, 1.5)),
        ),
        (
            "eq4_190_flat",
            current_default.with_post_eq_4(AudioEqStage::peaking(190.0, 0.90, 0.0)),
        ),
        (
            "lowshelf_m7p5_eq4_p1",
            current_default
                .with_post_eq_1(AudioEqStage::low_shelf(110.0, -7.5))
                .with_post_eq_4(AudioEqStage::peaking(190.0, 0.90, 1.0)),
        ),
        (
            "lowshelf_m8_eq4_0",
            current_default
                .with_post_eq_1(AudioEqStage::low_shelf(110.0, -8.0))
                .with_post_eq_4(AudioEqStage::peaking(190.0, 0.90, 0.0)),
        ),
        (
            "no_side_eq",
            current_default
                .with_side_gain(1.0)
                .with_post_side_eq_1(AudioEqStage::peaking(420.0, 0.95, 0.0))
                .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 0.0)),
        ),
        (
            "side_eq_presence",
            current_default
                .with_side_gain(1.02)
                .with_post_side_eq_1(AudioEqStage::peaking(420.0, 0.95, -1.6))
                .with_post_side_eq_2(AudioEqStage::peaking(900.0, 1.00, 1.3)),
        ),
        (
            "side_eq_trim",
            current_default
                .with_side_gain(0.98)
                .with_post_side_eq_1(AudioEqStage::peaking(400.0, 0.85, -1.8))
                .with_post_side_eq_2(AudioEqStage::peaking(2_100.0, 0.90, -1.0)),
        ),
        (
            "side_eq_combo",
            current_default
                .with_side_gain(1.01)
                .with_post_side_eq_1(AudioEqStage::peaking(410.0, 0.90, -1.7))
                .with_post_side_eq_2(AudioEqStage::peaking(880.0, 1.05, 1.2)),
        ),
    ];

    let mut base_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let base_rendered_full =
        base_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(base_rendered_full.len());
    let base_rendered = &base_rendered_full[capture_start..];
    let frame_len = 2048usize;
    let hop_len = 1024usize;
    let spectral_bins = log_frequency_bins(frame_len, SAMPLE_RATE, 80.0, 12_000.0, 24);

    let mut selections = Vec::new();
    let mut weights = Vec::new();
    let mut ref_left_specs = Vec::new();
    let mut ref_mid_specs = Vec::new();
    let mut ref_side_specs = Vec::new();
    for (_, ref_samples) in &loaded_refs {
        let Some(selection) = select_trusted_reference_window(base_rendered, ref_samples) else {
            continue;
        };
        let weight = trusted_window_reliability_weight(selection);
        let ref_window = stereo_window(
            ref_samples,
            selection.trusted_ref_start,
            selection.trusted_len,
        );
        let (ref_left, ref_mid, ref_side) =
            stereo_log_spectra(ref_window, frame_len, hop_len, &spectral_bins);
        selections.push(selection);
        weights.push(weight);
        ref_left_specs.push(ref_left);
        ref_mid_specs.push(ref_mid);
        ref_side_specs.push(ref_side);
    }

    if selections.is_empty() {
        eprintln!("No trusted reference selections available, skipping");
        return;
    }

    let consensus_left = weighted_average_vectors(&ref_left_specs, &weights);
    let consensus_mid = weighted_average_vectors(&ref_mid_specs, &weights);
    let consensus_side = weighted_average_vectors(&ref_side_specs, &weights);
    let ln_to_db = 10.0 / std::f32::consts::LN_10;

    eprintln!("=== GHZ reference-normalized candidate sweep ===");
    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];

        let mut left_cos = 0.0f32;
        let mut mid_cos = 0.0f32;
        let mut side_cos = 0.0f32;
        let mut left_mad = 0.0f32;
        let mut mid_mad = 0.0f32;
        let mut side_mad = 0.0f32;
        let mut weight_sum = 0.0f32;

        for (selection, &weight) in selections.iter().zip(&weights) {
            let emu_window =
                stereo_window(rendered, selection.trusted_emu_start, selection.trusted_len);
            let (emu_left, emu_mid, emu_side) =
                stereo_log_spectra(emu_window, frame_len, hop_len, &spectral_bins);
            left_cos += cosine_similarity(&emu_left, &consensus_left) * weight;
            mid_cos += cosine_similarity(&emu_mid, &consensus_mid) * weight;
            side_cos += cosine_similarity(&emu_side, &consensus_side) * weight;
            left_mad += mean_abs_delta(&emu_left, &consensus_left) * ln_to_db * weight;
            mid_mad += mean_abs_delta(&emu_mid, &consensus_mid) * ln_to_db * weight;
            side_mad += mean_abs_delta(&emu_side, &consensus_side) * ln_to_db * weight;
            weight_sum += weight;
        }

        if weight_sum <= 1e-6 {
            continue;
        }

        left_cos /= weight_sum;
        mid_cos /= weight_sum;
        side_cos /= weight_sum;
        left_mad /= weight_sum;
        mid_mad /= weight_sum;
        side_mad /= weight_sum;
        let score =
            (left_cos + mid_cos + side_cos) / 3.0 - 0.02 * (left_mad + mid_mad + side_mad) / 3.0;

        eprintln!(
            "{name:>22}: left=({left_cos:.4}/{left_mad:.2}dB) mid=({mid_cos:.4}/{mid_mad:.2}dB) side=({side_cos:.4}/{side_mad:.2}dB) score={score:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_reference_residual_stability() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let mut renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let rendered_full =
        renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(rendered_full.len());
    let rendered = &rendered_full[capture_start..];

    let frame_len = 2048usize;
    let hop_len = 1024usize;
    let segment_len = SAMPLE_RATE as usize;
    let spectral_bins = log_frequency_bins(frame_len, SAMPLE_RATE, 80.0, 12_000.0, 24);
    let ln_to_db = 10.0 / std::f32::consts::LN_10;
    let format_deltas = |deltas: &[(f32, f32)]| -> String {
        deltas
            .iter()
            .map(|(hz, db)| format!("{hz:.0}Hz {db:+.2}dB"))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let mut names = Vec::new();
    let mut weights = Vec::new();
    let mut left_means = Vec::new();
    let mut mid_means = Vec::new();
    let mut side_means = Vec::new();

    eprintln!("=== GHZ reference residual stability ===");
    for (ref_path, ref_samples) in &loaded_refs {
        let Some(selection) = select_trusted_reference_window(rendered, ref_samples) else {
            continue;
        };

        let ref_stereo = stereo_window(
            ref_samples,
            selection.trusted_ref_start,
            selection.trusted_len,
        );
        let emu_stereo =
            stereo_window(rendered, selection.trusted_emu_start, selection.trusted_len);
        let (ref_left, _) = stereo_left_right(ref_stereo);
        let (emu_left, _) = stereo_left_right(emu_stereo);
        let (ref_mid, ref_side) = stereo_mid_side(ref_stereo);
        let (emu_mid, emu_side) = stereo_mid_side(emu_stereo);

        let left_profiles = segmented_residual_profiles(
            &emu_left,
            &ref_left,
            segment_len,
            frame_len,
            hop_len,
            &spectral_bins,
        );
        let mid_profiles = segmented_residual_profiles(
            &emu_mid,
            &ref_mid,
            segment_len,
            frame_len,
            hop_len,
            &spectral_bins,
        );
        let side_profiles = segmented_residual_profiles(
            &emu_side,
            &ref_side,
            segment_len,
            frame_len,
            hop_len,
            &spectral_bins,
        );
        if left_profiles.is_empty() || mid_profiles.is_empty() || side_profiles.is_empty() {
            continue;
        }

        let uniform_weights = vec![1.0; left_profiles.len()];
        let left_mean = weighted_average_vectors(&left_profiles, &uniform_weights);
        let mid_mean = weighted_average_vectors(&mid_profiles, &uniform_weights);
        let side_mean = weighted_average_vectors(&side_profiles, &uniform_weights);

        let left_stability_db = residual_profile_stability(&left_profiles) * ln_to_db;
        let mid_stability_db = residual_profile_stability(&mid_profiles) * ln_to_db;
        let side_stability_db = residual_profile_stability(&side_profiles) * ln_to_db;
        let zero_left = vec![0.0; left_mean.len()];
        let zero_mid = vec![0.0; mid_mean.len()];
        let zero_side = vec![0.0; side_mean.len()];
        let name = ref_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_owned();
        let weight = trusted_window_reliability_weight(selection);

        eprintln!(
            "{name:>20}: segments={} weight={weight:.4} left_stability={left_stability_db:.2}dB mid_stability={mid_stability_db:.2}dB side_stability={side_stability_db:.2}dB",
            left_profiles.len()
        );
        eprintln!(
            "  mean residual left: {}",
            format_deltas(&largest_band_deltas_hz(
                &zero_left,
                &left_mean,
                frame_len,
                SAMPLE_RATE,
                &spectral_bins,
                5,
            ))
        );
        eprintln!(
            "  mean residual mid:  {}",
            format_deltas(&largest_band_deltas_hz(
                &zero_mid,
                &mid_mean,
                frame_len,
                SAMPLE_RATE,
                &spectral_bins,
                5,
            ))
        );
        eprintln!(
            "  mean residual side: {}",
            format_deltas(&largest_band_deltas_hz(
                &zero_side,
                &side_mean,
                frame_len,
                SAMPLE_RATE,
                &spectral_bins,
                5,
            ))
        );

        names.push(name);
        weights.push(weight);
        left_means.push(left_mean);
        mid_means.push(mid_mean);
        side_means.push(side_mean);
    }

    if names.len() < 2 {
        eprintln!("Not enough GHZ residual summaries for cross-reference comparison");
        return;
    }

    let consensus_left = weighted_average_vectors(&left_means, &weights);
    let consensus_mid = weighted_average_vectors(&mid_means, &weights);
    let consensus_side = weighted_average_vectors(&side_means, &weights);
    eprintln!("cross-reference residual consensus:");
    for idx in 0..names.len() {
        let left_cos = cosine_similarity(&left_means[idx], &consensus_left);
        let mid_cos = cosine_similarity(&mid_means[idx], &consensus_mid);
        let side_cos = cosine_similarity(&side_means[idx], &consensus_side);
        let left_mad_db = mean_abs_delta(&left_means[idx], &consensus_left) * ln_to_db;
        let mid_mad_db = mean_abs_delta(&mid_means[idx], &consensus_mid) * ln_to_db;
        let side_mad_db = mean_abs_delta(&side_means[idx], &consensus_side) * ln_to_db;
        eprintln!(
            "{:>20}: left=({left_cos:.4}/{left_mad_db:.2}dB) mid=({mid_cos:.4}/{mid_mad_db:.2}dB) side=({side_cos:.4}/{side_mad_db:.2}dB)",
            names[idx]
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_trusted_window_sections() {
    #[derive(Debug, Clone)]
    struct SectionMetrics {
        start: usize,
        len: usize,
        left: f32,
        mid: f32,
        side: f32,
        rms_ratio: f32,
        left_residual: Vec<f32>,
        mid_residual: Vec<f32>,
        side_residual: Vec<f32>,
    }

    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let mut renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let rendered_full =
        renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(rendered_full.len());
    let rendered = &rendered_full[capture_start..];

    let frame_len = 2048usize;
    let hop_len = 1024usize;
    let section_len = SAMPLE_RATE as usize;
    let spectral_bins = log_frequency_bins(frame_len, SAMPLE_RATE, 80.0, 12_000.0, 24);
    let ln_to_db = 10.0 / std::f32::consts::LN_10;
    let weighted_mad_db = |profiles: &[Vec<f32>], consensus: &[f32], weights: &[f32]| -> f32 {
        let weight_sum: f32 = weights.iter().sum();
        if weight_sum <= 1e-6 {
            return 0.0;
        }
        profiles
            .iter()
            .zip(weights)
            .map(|(profile, &weight)| mean_abs_delta(profile, consensus) * weight)
            .sum::<f32>()
            / weight_sum
            * ln_to_db
    };
    let format_deltas = |deltas: &[(f32, f32)]| -> String {
        deltas
            .iter()
            .map(|(hz, db)| format!("{hz:.0}Hz {db:+.2}dB"))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let mut per_reference_sections: Vec<(String, f32, Vec<SectionMetrics>)> = Vec::new();
    for (ref_path, ref_samples) in &loaded_refs {
        let Some(selection) = select_trusted_reference_window(rendered, ref_samples) else {
            continue;
        };

        let mut sections = Vec::new();
        for (start, len) in partition_window_sections(selection.trusted_len, section_len) {
            if len < frame_len {
                continue;
            }

            let ref_stereo = stereo_window(ref_samples, selection.trusted_ref_start + start, len);
            let emu_stereo = stereo_window(rendered, selection.trusted_emu_start + start, len);
            let (ref_left, _) = stereo_left_right(ref_stereo);
            let (emu_left, _) = stereo_left_right(emu_stereo);
            let (ref_mid, ref_side) = stereo_mid_side(ref_stereo);
            let (emu_mid, emu_side) = stereo_mid_side(emu_stereo);

            let ref_left_spec = average_log_spectrum(&ref_left, frame_len, hop_len, &spectral_bins);
            let emu_left_spec = average_log_spectrum(&emu_left, frame_len, hop_len, &spectral_bins);
            let ref_mid_spec = average_log_spectrum(&ref_mid, frame_len, hop_len, &spectral_bins);
            let emu_mid_spec = average_log_spectrum(&emu_mid, frame_len, hop_len, &spectral_bins);
            let ref_side_spec = average_log_spectrum(&ref_side, frame_len, hop_len, &spectral_bins);
            let emu_side_spec = average_log_spectrum(&emu_side, frame_len, hop_len, &spectral_bins);

            sections.push(SectionMetrics {
                start,
                len,
                left: cosine_similarity(&emu_left_spec, &ref_left_spec),
                mid: cosine_similarity(&emu_mid_spec, &ref_mid_spec),
                side: cosine_similarity(&emu_side_spec, &ref_side_spec),
                rms_ratio: rms(&emu_left) / rms(&ref_left).max(1e-9),
                left_residual: ref_left_spec
                    .iter()
                    .zip(&emu_left_spec)
                    .map(|(&ref_bin, &emu_bin)| ref_bin - emu_bin)
                    .collect(),
                mid_residual: ref_mid_spec
                    .iter()
                    .zip(&emu_mid_spec)
                    .map(|(&ref_bin, &emu_bin)| ref_bin - emu_bin)
                    .collect(),
                side_residual: ref_side_spec
                    .iter()
                    .zip(&emu_side_spec)
                    .map(|(&ref_bin, &emu_bin)| ref_bin - emu_bin)
                    .collect(),
            });
        }

        if sections.is_empty() {
            continue;
        }

        per_reference_sections.push((
            ref_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_owned(),
            trusted_window_reliability_weight(selection),
            sections,
        ));
    }

    if per_reference_sections.is_empty() {
        eprintln!("No trusted GHZ sections available, skipping");
        return;
    }

    let section_count = per_reference_sections
        .iter()
        .map(|(_, _, sections)| sections.len())
        .min()
        .unwrap_or(0);
    if section_count == 0 {
        eprintln!("No common GHZ sections available, skipping");
        return;
    }

    let mut ranked_sections = Vec::new();
    eprintln!("=== GHZ trusted-window sections ===");
    for idx in 0..section_count {
        let start = per_reference_sections[0].2[idx].start;
        let len = per_reference_sections[0].2[idx].len;
        let mut left_avg = 0.0f32;
        let mut mid_avg = 0.0f32;
        let mut side_avg = 0.0f32;
        let mut rms_fit_avg = 0.0f32;
        let mut weights = Vec::new();
        let mut left_profiles = Vec::new();
        let mut mid_profiles = Vec::new();
        let mut side_profiles = Vec::new();
        let mut weight_sum = 0.0f32;

        for (_, weight, sections) in &per_reference_sections {
            let section = &sections[idx];
            left_avg += section.left * *weight;
            mid_avg += section.mid * *weight;
            side_avg += section.side * *weight;
            rms_fit_avg += rms_fit_score(section.rms_ratio) * *weight;
            weights.push(*weight);
            left_profiles.push(section.left_residual.clone());
            mid_profiles.push(section.mid_residual.clone());
            side_profiles.push(section.side_residual.clone());
            weight_sum += *weight;
        }

        if weight_sum <= 1e-6 {
            continue;
        }

        left_avg /= weight_sum;
        mid_avg /= weight_sum;
        side_avg /= weight_sum;
        rms_fit_avg /= weight_sum;

        let left_consensus = weighted_average_vectors(&left_profiles, &weights);
        let mid_consensus = weighted_average_vectors(&mid_profiles, &weights);
        let side_consensus = weighted_average_vectors(&side_profiles, &weights);
        let left_disagree = weighted_mad_db(&left_profiles, &left_consensus, &weights);
        let mid_disagree = weighted_mad_db(&mid_profiles, &mid_consensus, &weights);
        let side_disagree = weighted_mad_db(&side_profiles, &side_consensus, &weights);
        let avg_score = (left_avg + mid_avg + side_avg + rms_fit_avg) / 4.0;
        let disagreement_penalty = 0.02 * (left_disagree + mid_disagree + side_disagree) / 3.0;
        let final_score = avg_score - disagreement_penalty;
        let zero_mid = vec![0.0; mid_consensus.len()];
        let mid_residual = format_deltas(&largest_band_deltas_hz(
            &zero_mid,
            &mid_consensus,
            frame_len,
            SAMPLE_RATE,
            &spectral_bins,
            3,
        ));

        eprintln!(
            "section {:>2} [{:.2}s..{:.2}s]: avg={avg_score:.4} final={final_score:.4} left={left_avg:.4} mid={mid_avg:.4} side={side_avg:.4} rms={rms_fit_avg:.4} disagree={:.2}dB",
            idx,
            start as f64 / SAMPLE_RATE as f64,
            (start + len) as f64 / SAMPLE_RATE as f64,
            (left_disagree + mid_disagree + side_disagree) / 3.0,
        );
        eprintln!("  consensus mid residual: {mid_residual}");

        ranked_sections.push((idx, start, len, final_score));
    }

    ranked_sections.sort_by(|a, b| a.3.partial_cmp(&b.3).unwrap_or(std::cmp::Ordering::Equal));
    if let Some((worst_idx, worst_start, worst_len, worst_score)) = ranked_sections.first().copied()
    {
        eprintln!(
            "worst section: {} [{:.2}s..{:.2}s] final={worst_score:.4}",
            worst_idx,
            worst_start as f64 / SAMPLE_RATE as f64,
            (worst_start + worst_len) as f64 / SAMPLE_RATE as f64,
        );
        for (name, _, sections) in &per_reference_sections {
            let section = &sections[worst_idx];
            let score =
                (section.left + section.mid + section.side + rms_fit_score(section.rms_ratio))
                    / 4.0;
            eprintln!(
                "  {:>20}: score={score:.4} left={:.4} mid={:.4} side={:.4} rms={:.4}",
                name,
                section.left,
                section.mid,
                section.side,
                rms_fit_score(section.rms_ratio),
            );
        }
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_worst_section_local_realign() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let mut renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let rendered_full =
        renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(rendered_full.len());
    let rendered = &rendered_full[capture_start..];

    let section_len = SAMPLE_RATE as usize;
    let mut references = Vec::new();
    for (ref_path, ref_samples) in &loaded_refs {
        let Some(selection) = select_trusted_reference_window(rendered, ref_samples) else {
            continue;
        };
        references.push((
            ref_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_owned(),
            selection,
            ref_samples,
            trusted_window_reliability_weight(selection),
        ));
    }
    if references.is_empty() {
        eprintln!("No trusted GHZ sections available, skipping");
        return;
    }

    let section_count = references
        .iter()
        .map(|(_, selection, _, _)| {
            partition_window_sections(selection.trusted_len, section_len).len()
        })
        .min()
        .unwrap_or(0);
    if section_count == 0 {
        eprintln!("No common GHZ sections available, skipping");
        return;
    }

    let mut worst_section = None;
    for idx in 0..section_count {
        let mut score_sum = 0.0f32;
        let mut weight_sum = 0.0f32;
        for (_, selection, ref_samples, weight) in &references {
            let start = idx * section_len;
            let len = section_len.min(selection.trusted_len.saturating_sub(start));
            if len < 2048 {
                continue;
            }
            let metrics = score_aligned_section(
                rendered,
                ref_samples,
                selection.trusted_emu_start + start,
                selection.trusted_ref_start + start,
                len,
            )
            .expect("section metrics");
            score_sum += fixed_window_fit_score(metrics) * *weight;
            weight_sum += *weight;
        }
        if weight_sum <= 1e-6 {
            continue;
        }
        let final_score = score_sum / weight_sum;
        if worst_section.is_none_or(|(_, best_score)| final_score < best_score) {
            worst_section = Some((idx, final_score));
        }
    }

    let Some((worst_idx, worst_score)) = worst_section else {
        eprintln!("Could not identify worst GHZ section");
        return;
    };
    let worst_start = worst_idx * section_len;

    eprintln!(
        "=== GHZ worst-section local realign === section={} [{:.2}s..{:.2}s] baseline={worst_score:.4}",
        worst_idx,
        worst_start as f64 / SAMPLE_RATE as f64,
        (worst_start + section_len) as f64 / SAMPLE_RATE as f64,
    );
    let rendered_left: Vec<f32> = rendered.iter().step_by(2).copied().collect();
    for (name, selection, ref_samples, _) in references {
        let len = section_len.min(selection.trusted_len.saturating_sub(worst_start));
        if len < 2048 {
            continue;
        }

        let nominal_emu_start = selection.trusted_emu_start + worst_start;
        let nominal_ref_start = selection.trusted_ref_start + worst_start;
        let nominal = score_aligned_section(
            rendered,
            ref_samples,
            nominal_emu_start,
            nominal_ref_start,
            len,
        )
        .expect("nominal section metrics");
        let ref_left: Vec<f32> = ref_samples.iter().step_by(2).copied().collect();
        let (offset, env_corr) = best_reference_section_offset(
            &rendered_left,
            &ref_left,
            nominal_emu_start,
            nominal_ref_start,
            len,
            256,
            16,
        )
        .expect("local offset search");
        let adjusted_ref_start = if offset >= 0 {
            nominal_ref_start + offset as usize
        } else {
            nominal_ref_start - (-offset) as usize
        };
        let adjusted = score_aligned_section(
            rendered,
            ref_samples,
            nominal_emu_start,
            adjusted_ref_start,
            len,
        )
        .expect("adjusted section metrics");

        eprintln!(
            "{name:>20}: offset={:+} samples ({:+.2} ms) env_corr={env_corr:.4} nominal={:.4} adjusted={:.4}",
            offset,
            offset as f64 * 1000.0 / SAMPLE_RATE as f64,
            fixed_window_fit_score(nominal),
            fixed_window_fit_score(adjusted),
        );
        eprintln!(
            "  nominal  left={:.4} mid={:.4} side={:.4} rms={:.4}",
            nominal.spectral,
            nominal.mid,
            nominal.side,
            rms_fit_score(nominal.rms_ratio),
        );
        eprintln!(
            "  adjusted left={:.4} mid={:.4} side={:.4} rms={:.4}",
            adjusted.spectral,
            adjusted.mid,
            adjusted.side,
            rms_fit_score(adjusted.rms_ratio),
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_worst_section_reference_integrity() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let mut renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let rendered_full =
        renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(rendered_full.len());
    let rendered = &rendered_full[capture_start..];

    let section_len = SAMPLE_RATE as usize;
    let spectral_bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 12_000.0, 24);
    let mut references = Vec::new();
    for (ref_path, ref_samples) in &loaded_refs {
        let Some(selection) = select_trusted_reference_window(rendered, ref_samples) else {
            continue;
        };
        references.push((
            ref_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_owned(),
            selection,
            ref_samples,
            trusted_window_reliability_weight(selection),
        ));
    }
    if references.is_empty() {
        eprintln!("No trusted GHZ references available, skipping");
        return;
    }

    let section_count = references
        .iter()
        .map(|(_, selection, _, _)| {
            partition_window_sections(selection.trusted_len, section_len).len()
        })
        .min()
        .unwrap_or(0);
    if section_count == 0 {
        eprintln!("No common GHZ sections available, skipping");
        return;
    }

    let mut worst_section = None;
    for idx in 0..section_count {
        let mut score_sum = 0.0f32;
        let mut weight_sum = 0.0f32;
        for (_, selection, ref_samples, weight) in &references {
            let start = idx * section_len;
            let len = section_len.min(selection.trusted_len.saturating_sub(start));
            if len < 2048 {
                continue;
            }
            let metrics = score_aligned_section(
                rendered,
                ref_samples,
                selection.trusted_emu_start + start,
                selection.trusted_ref_start + start,
                len,
            )
            .expect("section metrics");
            score_sum += fixed_window_fit_score(metrics) * *weight;
            weight_sum += *weight;
        }
        if weight_sum <= 1e-6 {
            continue;
        }
        let final_score = score_sum / weight_sum;
        if worst_section.is_none_or(|(_, best_score)| final_score < best_score) {
            worst_section = Some((idx, final_score));
        }
    }

    let Some((worst_idx, worst_score)) = worst_section else {
        eprintln!("Could not identify worst GHZ section");
        return;
    };
    let worst_start = worst_idx * section_len;

    eprintln!(
        "=== GHZ worst-section reference integrity === section={} [{:.2}s..{:.2}s] weighted={worst_score:.4}",
        worst_idx,
        worst_start as f64 / SAMPLE_RATE as f64,
        (worst_start + section_len) as f64 / SAMPLE_RATE as f64,
    );
    for (name, selection, ref_samples, _) in references {
        let len = section_len.min(selection.trusted_len.saturating_sub(worst_start));
        if len < 2048 {
            continue;
        }

        let current_start = selection.trusted_ref_start + worst_start;
        let prior_start = selection.trusted_prior_start + worst_start;
        let nominal_self = reference_self_window_at_offset(
            ref_samples,
            prior_start,
            current_start,
            len,
            2048,
            1024,
            &spectral_bins,
        )
        .expect("nominal self-match");
        let ref_left: Vec<f32> = ref_samples.iter().step_by(2).copied().collect();
        let (offset, env_corr) = best_reference_section_offset(
            &ref_left,
            &ref_left,
            current_start,
            prior_start,
            len,
            256,
            16,
        )
        .expect("self local offset search");
        let adjusted_prior_start = if offset >= 0 {
            prior_start + offset as usize
        } else {
            prior_start - (-offset) as usize
        };
        let adjusted_self = reference_self_window_at_offset(
            ref_samples,
            adjusted_prior_start,
            current_start,
            len,
            2048,
            1024,
            &spectral_bins,
        )
        .expect("adjusted self-match");

        let current_stereo = stereo_window(ref_samples, current_start, len);
        let (left, right) = stereo_left_right(current_stereo);
        let left_peak = left.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        let right_peak = right.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        let left_dc = left.iter().copied().sum::<f32>() / left.len().max(1) as f32;
        let right_dc = right.iter().copied().sum::<f32>() / right.len().max(1) as f32;
        let left_repeat = repeated_consecutive_ratio(&left) * 100.0;
        let right_repeat = repeated_consecutive_ratio(&right) * 100.0;
        let left_clip = count_near_clipped(&left, 0.98);
        let right_clip = count_near_clipped(&right, 0.98);
        let (left_mean_delta, left_max_delta, left_jumps) = sample_jump_metrics(&left, 0.1);
        let (right_mean_delta, right_max_delta, right_jumps) = sample_jump_metrics(&right, 0.1);
        let entry_jump = if current_start > 0 {
            (
                (ref_left[current_start] - ref_left[current_start - 1]).abs(),
                (ref_samples[current_start * 2 + 1] - ref_samples[(current_start - 1) * 2 + 1])
                    .abs(),
            )
        } else {
            (0.0, 0.0)
        };
        let exit_idx = current_start + len;
        let exit_jump = if exit_idx < ref_left.len() {
            (
                (ref_left[exit_idx] - ref_left[exit_idx - 1]).abs(),
                (ref_samples[exit_idx * 2 + 1] - ref_samples[(exit_idx - 1) * 2 + 1]).abs(),
            )
        } else {
            (0.0, 0.0)
        };

        eprintln!(
            "{name:>20}: self_nominal={:.4} self_adjusted={:.4} offset={:+} samples ({:+.2} ms) env_corr={env_corr:.4}",
            nominal_self.score,
            adjusted_self.score,
            offset,
            offset as f64 * 1000.0 / SAMPLE_RATE as f64,
        );
        eprintln!(
            "  self nominal  left={:.4} mid={:.4} side={:.4} rms={:.4}",
            nominal_self.left_spectral,
            nominal_self.mid_spectral,
            nominal_self.side_spectral,
            rms_fit_score(nominal_self.rms_ratio),
        );
        eprintln!(
            "  self adjusted left={:.4} mid={:.4} side={:.4} rms={:.4}",
            adjusted_self.left_spectral,
            adjusted_self.mid_spectral,
            adjusted_self.side_spectral,
            rms_fit_score(adjusted_self.rms_ratio),
        );
        eprintln!(
            "  waveform left/right: peak=({left_peak:.4}/{right_peak:.4}) dc=({left_dc:.4}/{right_dc:.4}) lr_corr={:.4} side_ratio={:.4}",
            stereo_lr_correlation(current_stereo),
            stereo_side_ratio(current_stereo),
        );
        eprintln!(
            "  repeats left/right: {left_repeat:.2}% / {right_repeat:.2}%   near_clip: {left_clip} / {right_clip}"
        );
        eprintln!(
            "  deltas left/right: mean=({left_mean_delta:.5}/{right_mean_delta:.5}) max=({left_max_delta:.5}/{right_max_delta:.5}) jumps>0.1=({left_jumps}/{right_jumps})"
        );
        eprintln!(
            "  boundary jumps L/R: entry=({:.5}/{:.5}) exit=({:.5}/{:.5})",
            entry_jump.0, entry_jump.1, exit_jump.0, exit_jump.1,
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_worst_section_cross_capture_alignment() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let mut renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let rendered_full =
        renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(rendered_full.len());
    let rendered = &rendered_full[capture_start..];

    let section_len = SAMPLE_RATE as usize;
    let mut references = Vec::new();
    for (ref_path, ref_samples) in &loaded_refs {
        let Some(selection) = select_trusted_reference_window(rendered, ref_samples) else {
            continue;
        };
        references.push((
            ref_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_owned(),
            selection,
            ref_samples,
            trusted_window_reliability_weight(selection),
        ));
    }
    if references.len() < 2 {
        eprintln!("Need at least two trusted GHZ references, skipping");
        return;
    }

    let section_count = references
        .iter()
        .map(|(_, selection, _, _)| {
            partition_window_sections(selection.trusted_len, section_len).len()
        })
        .min()
        .unwrap_or(0);
    if section_count == 0 {
        eprintln!("No common GHZ sections available, skipping");
        return;
    }

    let mut worst_section = None;
    for idx in 0..section_count {
        let mut score_sum = 0.0f32;
        let mut weight_sum = 0.0f32;
        for (_, selection, ref_samples, weight) in &references {
            let start = idx * section_len;
            let len = section_len.min(selection.trusted_len.saturating_sub(start));
            if len < 2048 {
                continue;
            }
            let metrics = score_aligned_section(
                rendered,
                ref_samples,
                selection.trusted_emu_start + start,
                selection.trusted_ref_start + start,
                len,
            )
            .expect("section metrics");
            score_sum += fixed_window_fit_score(metrics) * *weight;
            weight_sum += *weight;
        }
        if weight_sum <= 1e-6 {
            continue;
        }
        let final_score = score_sum / weight_sum;
        if worst_section.is_none_or(|(_, best_score)| final_score < best_score) {
            worst_section = Some((idx, final_score));
        }
    }

    let Some((worst_idx, worst_score)) = worst_section else {
        eprintln!("Could not identify worst GHZ section");
        return;
    };
    let worst_start = worst_idx * section_len;

    references.sort_by(|a, b| a.0.cmp(&b.0));
    let (anchor_name, anchor_selection, anchor_samples, _) = &references[0];
    let (other_name, other_selection, other_samples, _) = &references[1];
    let len = section_len
        .min(anchor_selection.trusted_len.saturating_sub(worst_start))
        .min(other_selection.trusted_len.saturating_sub(worst_start));
    if len < 2048 {
        eprintln!("Worst GHZ section too short for cross-capture comparison");
        return;
    }

    let anchor_start = anchor_selection.trusted_ref_start + worst_start;
    let other_start = other_selection.trusted_ref_start + worst_start;
    let nominal = score_aligned_section(
        anchor_samples,
        other_samples,
        anchor_start,
        other_start,
        len,
    )
    .expect("cross-capture nominal score");
    let anchor_left: Vec<f32> = anchor_samples.iter().step_by(2).copied().collect();
    let other_left: Vec<f32> = other_samples.iter().step_by(2).copied().collect();
    let (offset, env_corr) = best_reference_section_offset(
        &anchor_left,
        &other_left,
        anchor_start,
        other_start,
        len,
        256,
        16,
    )
    .expect("cross-capture local offset");
    let adjusted_other_start = if offset >= 0 {
        other_start + offset as usize
    } else {
        other_start - (-offset) as usize
    };
    let adjusted = score_aligned_section(
        anchor_samples,
        other_samples,
        anchor_start,
        adjusted_other_start,
        len,
    )
    .expect("cross-capture adjusted score");

    eprintln!(
        "=== GHZ worst-section cross-capture alignment === section={} [{:.2}s..{:.2}s] weighted={worst_score:.4}",
        worst_idx,
        worst_start as f64 / SAMPLE_RATE as f64,
        (worst_start + len) as f64 / SAMPLE_RATE as f64,
    );
    eprintln!(
        "{anchor_name:>20} vs {other_name}: offset={:+} samples ({:+.2} ms) env_corr={env_corr:.4} nominal={:.4} adjusted={:.4}",
        offset,
        offset as f64 * 1000.0 / SAMPLE_RATE as f64,
        fixed_window_fit_score(nominal),
        fixed_window_fit_score(adjusted),
    );
    eprintln!(
        "  nominal  left={:.4} mid={:.4} side={:.4} rms={:.4}",
        nominal.spectral,
        nominal.mid,
        nominal.side,
        rms_fit_score(nominal.rms_ratio),
    );
    eprintln!(
        "  adjusted left={:.4} mid={:.4} side={:.4} rms={:.4}",
        adjusted.spectral,
        adjusted.mid,
        adjusted.side,
        rms_fit_score(adjusted.rms_ratio),
    );
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_sectioned_consensus_profiles() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            eprintln!("Skipping {} due to sample rate {}", path.display(), rate);
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let section_len = SAMPLE_RATE as usize;

    let mut scored = Vec::new();
    for (name, config) in consensus_profile_candidates() {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];

        let mut metrics = Vec::new();
        let mut sectioned = Vec::new();
        for (_, ref_samples) in &loaded_refs {
            let Some(selection) = select_trusted_reference_window(rendered, ref_samples) else {
                continue;
            };
            let Some(metric) = analyze_trusted_reference_window(rendered, ref_samples) else {
                continue;
            };
            let Some(sectioned_score) =
                score_sectioned_reference(rendered, ref_samples, selection, section_len)
            else {
                continue;
            };
            metrics.push(metric);
            sectioned.push(sectioned_score);
        }
        if metrics.is_empty() {
            continue;
        }
        scored.push(score_sectioned_consensus_candidate(
            name, &metrics, &sectioned,
        ));
    }

    scored.sort_by(|a, b| {
        b.final_score
            .partial_cmp(&a.final_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.average_score
                    .partial_cmp(&a.average_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    eprintln!("=== GHZ sectioned consensus profile sweep ===");
    for score in scored {
        eprintln!(
            "{:>20}: average={:.4} penalty={:.4} final={:.4}",
            score.name, score.average_score, score.disagreement_penalty, score.final_score
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_sectioned_consensus_leaders() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let section_len = SAMPLE_RATE as usize;
    let candidates = [
        ("current_default", AudioOutputConfig::default()),
        (
            "xf_0_30",
            AudioOutputConfig::default().with_stereo_crossfeed(0.30),
        ),
        ("psg_0_90", AudioOutputConfig::default().with_psg_gain(0.90)),
    ];

    eprintln!("=== GHZ sectioned consensus leaders ===");
    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];

        let mut metrics = Vec::new();
        let mut sectioned = Vec::new();
        eprintln!("-- {name} --");
        for (ref_path, ref_samples) in &loaded_refs {
            let ref_name = ref_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");
            let Some(selection) = select_trusted_reference_window(rendered, ref_samples) else {
                continue;
            };
            let Some(metric) = analyze_trusted_reference_window(rendered, ref_samples) else {
                continue;
            };
            let Some(sectioned_score) =
                score_sectioned_reference(rendered, ref_samples, selection, section_len)
            else {
                continue;
            };
            eprintln!(
                "{ref_name:>20}: ref_weight={:.4} section_weight={:.4} global_norm={:.4} section_norm={:.4} worst_section={:.4}",
                reference_reliability_weight(metric),
                sectioned_score.section_reliability,
                normalized_reference_score(metric),
                sectioned_score.normalized_score,
                sectioned_score.worst_section_score,
            );
            metrics.push(metric);
            sectioned.push(sectioned_score);
        }
        let score = score_sectioned_consensus_candidate(name, &metrics, &sectioned);
        eprintln!(
            "{name:>20}: average={:.4} penalty={:.4} final={:.4}",
            score.average_score, score.disagreement_penalty, score.final_score
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_mono_first_consensus_profiles() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            eprintln!("Skipping {} due to sample rate {}", path.display(), rate);
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let section_len = SAMPLE_RATE as usize;

    let mut scored = Vec::new();
    for (name, config) in consensus_profile_candidates() {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];

        let mut metrics = Vec::new();
        let mut mono_first = Vec::new();
        for (_, ref_samples) in &loaded_refs {
            let Some(selection) = select_trusted_reference_window(rendered, ref_samples) else {
                continue;
            };
            let Some(metric) = analyze_trusted_reference_window(rendered, ref_samples) else {
                continue;
            };
            let Some(score) =
                score_mono_first_sectioned_reference(rendered, ref_samples, selection, section_len)
            else {
                continue;
            };
            metrics.push(metric);
            mono_first.push(score);
        }
        if metrics.is_empty() {
            continue;
        }
        scored.push(score_mono_first_consensus_candidate(
            name,
            &metrics,
            &mono_first,
        ));
    }

    scored.sort_by(|a, b| {
        b.final_score
            .partial_cmp(&a.final_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.average_score
                    .partial_cmp(&a.average_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    eprintln!("=== GHZ mono-first consensus profile sweep ===");
    for score in scored {
        eprintln!(
            "{:>20}: average={:.4} penalty={:.4} final={:.4}",
            score.name, score.average_score, score.disagreement_penalty, score.final_score
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_mono_first_consensus_leaders() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let section_len = SAMPLE_RATE as usize;
    let candidates = [
        ("current_default", AudioOutputConfig::default()),
        ("no_side_eq", calibrated_default_without_side_eq()),
        (
            "xf_0_30",
            AudioOutputConfig::default().with_stereo_crossfeed(0.30),
        ),
        ("psg_0_90", AudioOutputConfig::default().with_psg_gain(0.90)),
    ];

    eprintln!("=== GHZ mono-first consensus leaders ===");
    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];

        let mut metrics = Vec::new();
        let mut mono_first = Vec::new();
        eprintln!("-- {name} --");
        for (ref_path, ref_samples) in &loaded_refs {
            let ref_name = ref_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");
            let Some(selection) = select_trusted_reference_window(rendered, ref_samples) else {
                continue;
            };
            let Some(metric) = analyze_trusted_reference_window(rendered, ref_samples) else {
                continue;
            };
            let Some(score) =
                score_mono_first_sectioned_reference(rendered, ref_samples, selection, section_len)
            else {
                continue;
            };
            eprintln!(
                "{ref_name:>20}: ref_weight={:.4} section_weight={:.4} mono={:.4} side={:.4} combined={:.4} worst_mono={:.4} worst_side={:.4}",
                reference_reliability_weight(metric),
                score.section_reliability,
                score.mono_score,
                score.side_score,
                score.combined_score,
                score.worst_mono_section_score,
                score.worst_side_section_score,
            );
            metrics.push(metric);
            mono_first.push(score);
        }
        let score = score_mono_first_consensus_candidate(name, &metrics, &mono_first);
        eprintln!(
            "{name:>20}: average={:.4} penalty={:.4} final={:.4}",
            score.average_score, score.disagreement_penalty, score.final_score
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_mono_consensus_profiles() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            eprintln!("Skipping {} due to sample rate {}", path.display(), rate);
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let mut anchor_renderer =
        CoreAudioRenderer::with_audio_output_config(AudioOutputConfig::default());
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];
    let Some((target, fixed_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build mono consensus target");
        return;
    };

    let mut scored = Vec::new();
    for (name, config) in consensus_profile_candidates() {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        let Some(score) =
            score_fixed_mono_consensus_candidate(name, rendered, &fixed_refs, &target)
        else {
            continue;
        };
        scored.push(score);
    }

    scored.sort_by(|a, b| {
        b.final_score
            .partial_cmp(&a.final_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.average_score
                    .partial_cmp(&a.average_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    eprintln!(
        "=== GHZ mono consensus target === self_spectral={:.4} self_rms_fit={:.4} refs={}",
        target.self_spectral,
        target.self_rms_fit,
        fixed_refs.len()
    );
    eprintln!("=== GHZ mono consensus profile sweep ===");
    for score in scored {
        eprintln!(
            "{:>20}: average={:.4} penalty={:.4} final={:.4}",
            score.name, score.average_score, score.disagreement_penalty, score.final_score
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_mono_consensus_leaders() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let mut anchor_renderer =
        CoreAudioRenderer::with_audio_output_config(AudioOutputConfig::default());
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];
    let Some((target, fixed_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build mono consensus target");
        return;
    };
    let current_default = AudioOutputConfig::default();
    let candidates = [
        ("current_default", current_default),
        (
            "side_air_plus",
            current_default
                .with_post_side_eq_1(AudioEqStage::peaking(420.0, 0.95, -1.6))
                .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 1.6)),
        ),
        ("psg_0_90", current_default.with_psg_gain(0.90)),
        ("xf_0_30", current_default.with_stereo_crossfeed(0.30)),
    ];

    eprintln!(
        "=== GHZ mono consensus leaders === target_self_spectral={:.4} target_self_rms_fit={:.4}",
        target.self_spectral, target.self_rms_fit
    );
    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        let Some(windows) = extract_fixed_mono_consensus_candidate_windows(rendered, &fixed_refs)
        else {
            continue;
        };
        let windows_only: Vec<_> = windows.iter().map(|(_, window)| window.clone()).collect();
        let score = score_mono_consensus_candidate_windows(name, &windows_only, &target);
        eprintln!("-- {name} --");
        for (ref_name, window) in windows {
            let spectral = cosine_similarity(&window.spectrum, &target.spectrum);
            let rms_fit = rms_fit_score(window.rms / target.rms.max(1e-9));
            let combined = normalized_score(spectral, target.self_spectral) * 0.85
                + normalized_score(rms_fit, target.self_rms_fit) * 0.15;
            eprintln!(
                "{ref_name:>20}: weight={:.4} spectral={:.4} rms_fit={:.4} combined={:.4}",
                window.weight, spectral, rms_fit, combined
            );
        }
        eprintln!(
            "{name:>20}: average={:.4} penalty={:.4} final={:.4}",
            score.average_score, score.disagreement_penalty, score.final_score
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_hybrid_mono_consensus_profiles() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            eprintln!("Skipping {} due to sample rate {}", path.display(), rate);
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let mut anchor_renderer =
        CoreAudioRenderer::with_audio_output_config(AudioOutputConfig::default());
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];
    let Some((target, fixed_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build mono consensus target");
        return;
    };

    let mut scored = Vec::new();
    for (name, config) in consensus_profile_candidates() {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        let Some(score) = score_fixed_hybrid_mono_consensus_candidate(
            name,
            rendered,
            &fixed_refs,
            &loaded_refs,
            &target,
        ) else {
            continue;
        };
        scored.push(score);
    }

    scored.sort_by(|a, b| {
        b.final_score
            .partial_cmp(&a.final_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.average_score
                    .partial_cmp(&a.average_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    eprintln!(
        "=== GHZ hybrid mono consensus target === self_spectral={:.4} self_rms_fit={:.4}",
        target.self_spectral, target.self_rms_fit
    );
    eprintln!("=== GHZ hybrid mono consensus profile sweep ===");
    for score in scored {
        eprintln!(
            "{:>20}: average={:.4} penalty={:.4} final={:.4}",
            score.name, score.average_score, score.disagreement_penalty, score.final_score
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_hybrid_mono_consensus_leaders() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let mut anchor_renderer =
        CoreAudioRenderer::with_audio_output_config(AudioOutputConfig::default());
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];
    let Some((target, fixed_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build mono consensus target");
        return;
    };
    let current_default = AudioOutputConfig::default();
    let candidates = [
        ("current_default", current_default),
        (
            "side_air_plus",
            current_default
                .with_post_side_eq_1(AudioEqStage::peaking(420.0, 0.95, -1.6))
                .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 1.6)),
        ),
        ("psg_0_70", current_default.with_psg_gain(0.70)),
        ("psg_0_90", current_default.with_psg_gain(0.90)),
    ];

    eprintln!(
        "=== GHZ hybrid mono consensus leaders === target_self_spectral={:.4} target_self_rms_fit={:.4}",
        target.self_spectral, target.self_rms_fit
    );
    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        let Some(score) = score_fixed_hybrid_mono_consensus_candidate(
            name,
            rendered,
            &fixed_refs,
            &loaded_refs,
            &target,
        ) else {
            continue;
        };
        eprintln!("-- {name} --");
        for reference in &fixed_refs {
            let ref_samples = &loaded_refs[reference.ref_index].1;
            let Some(metrics) =
                score_fixed_trusted_window(rendered, ref_samples, reference.selection)
            else {
                continue;
            };
            let side_norm = normalized_score(metrics.side, reference.selection.trusted_self_side);
            eprintln!(
                "{:>20}: weight={:.4} side_norm={:.4}",
                reference.name, reference.weight, side_norm
            );
        }
        eprintln!(
            "{name:>20}: average={:.4} penalty={:.4} final={:.4}",
            score.average_score, score.disagreement_penalty, score.final_score
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_hybrid_mono_consensus_psg_sweep() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let mut anchor_renderer =
        CoreAudioRenderer::with_audio_output_config(AudioOutputConfig::default());
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];
    let Some((target, fixed_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build mono consensus target");
        return;
    };

    eprintln!(
        "=== GHZ hybrid mono consensus PSG sweep === target_self_spectral={:.4} target_self_rms_fit={:.4}",
        target.self_spectral, target.self_rms_fit
    );
    for gain in [0.60f32, 0.65, 0.70, 0.75, 0.80, 0.85, 0.90, 0.95, 1.00] {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(
            AudioOutputConfig::default().with_psg_gain(gain),
        );
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        let Some(score) = score_fixed_hybrid_mono_consensus_candidate(
            &format!("psg_{gain:.2}"),
            rendered,
            &fixed_refs,
            &loaded_refs,
            &target,
        ) else {
            continue;
        };
        eprintln!(
            "psg_gain={gain:.2}: average={:.4} penalty={:.4} final={:.4}",
            score.average_score, score.disagreement_penalty, score.final_score
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_hybrid_mono_consensus_crossfeed_sweep() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let mut anchor_renderer =
        CoreAudioRenderer::with_audio_output_config(AudioOutputConfig::default());
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];
    let Some((target, fixed_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build mono consensus target");
        return;
    };

    eprintln!(
        "=== GHZ hybrid mono consensus crossfeed sweep === target_self_spectral={:.4} target_self_rms_fit={:.4}",
        target.self_spectral, target.self_rms_fit
    );
    for crossfeed in [0.20f32, 0.25, 0.30, 0.35, 0.40, 0.45] {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(
            AudioOutputConfig::default().with_stereo_crossfeed(crossfeed),
        );
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        let Some(score) = score_fixed_hybrid_mono_consensus_candidate(
            &format!("xf_{crossfeed:.2}"),
            rendered,
            &fixed_refs,
            &loaded_refs,
            &target,
        ) else {
            continue;
        };
        eprintln!(
            "crossfeed={crossfeed:.2}: average={:.4} penalty={:.4} final={:.4}",
            score.average_score, score.disagreement_penalty, score.final_score
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_hybrid_mono_consensus_side_eq_sweep() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];
    let Some((target, fixed_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build mono consensus target");
        return;
    };

    let lowmid_db = [0.0f32, -0.8, -1.6, -2.4, -3.2];
    let presence_db = [0.0f32, 0.4, 0.8, 1.2, 1.6];
    let mut results = Vec::new();
    eprintln!(
        "=== GHZ hybrid mono consensus side EQ sweep === target_self_spectral={:.4} target_self_rms_fit={:.4}",
        target.self_spectral, target.self_rms_fit
    );
    for lowmid in lowmid_db {
        for presence in presence_db {
            let config = current_default
                .with_post_side_eq_1(AudioEqStage::peaking(420.0, 0.95, lowmid))
                .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, presence));
            let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
            let rendered_full = renderer.render_timed_writes(
                &trace.ym_writes,
                &trace.psg_writes,
                0,
                trace.end_tick,
            );
            let capture_start = (trace.capture_start_sample as usize)
                .saturating_mul(2)
                .min(rendered_full.len());
            let rendered = &rendered_full[capture_start..];
            let Some(score) = score_fixed_hybrid_mono_consensus_candidate(
                &format!("side_{lowmid:+.1}_{presence:+.1}"),
                rendered,
                &fixed_refs,
                &loaded_refs,
                &target,
            ) else {
                continue;
            };
            results.push((lowmid, presence, score));
        }
    }
    results.sort_by(|a, b| {
        b.2.final_score
            .partial_cmp(&a.2.final_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.2.average_score
                    .partial_cmp(&a.2.average_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    for (lowmid, presence, score) in results.into_iter().take(10) {
        eprintln!(
            "side_eq lowmid={lowmid:+.1} presence={presence:+.1}: average={:.4} penalty={:.4} final={:.4}",
            score.average_score, score.disagreement_penalty, score.final_score
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_hybrid_mono_consensus_side_lowmid_refine() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];
    let Some((target, fixed_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build mono consensus target");
        return;
    };

    let freqs_hz = [390.0f32, 420.0, 450.0, 480.0];
    let cuts_db = [-2.8f32, -3.2, -3.6, -4.0];
    let mut results = Vec::new();
    eprintln!(
        "=== GHZ hybrid mono consensus side lowmid refine === target_self_spectral={:.4} target_self_rms_fit={:.4}",
        target.self_spectral, target.self_rms_fit
    );
    for freq_hz in freqs_hz {
        for cut_db in cuts_db {
            let config = current_default
                .with_post_side_eq_1(AudioEqStage::peaking(freq_hz, 0.95, cut_db))
                .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 0.0));
            let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
            let rendered_full = renderer.render_timed_writes(
                &trace.ym_writes,
                &trace.psg_writes,
                0,
                trace.end_tick,
            );
            let capture_start = (trace.capture_start_sample as usize)
                .saturating_mul(2)
                .min(rendered_full.len());
            let rendered = &rendered_full[capture_start..];
            let Some(score) = score_fixed_hybrid_mono_consensus_candidate(
                &format!("side_{freq_hz:.0}_{cut_db:+.1}"),
                rendered,
                &fixed_refs,
                &loaded_refs,
                &target,
            ) else {
                continue;
            };
            results.push((freq_hz, cut_db, score));
        }
    }
    results.sort_by(|a, b| {
        b.2.final_score
            .partial_cmp(&a.2.final_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.2.average_score
                    .partial_cmp(&a.2.average_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    for (freq_hz, cut_db, score) in results.into_iter().take(10) {
        eprintln!(
            "side_lowmid freq={freq_hz:.0}Hz cut={cut_db:+.1}dB: average={:.4} penalty={:.4} final={:.4}",
            score.average_score, score.disagreement_penalty, score.final_score
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_refined_side_lowmid_candidate_vs_current() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let refined = current_default
        .with_post_side_eq_1(AudioEqStage::peaking(450.0, 0.95, -4.0))
        .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 0.0));
    let candidates = [
        ("current_default", current_default),
        ("refined_side_lowmid", refined),
    ];

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];
    let Some((target, fixed_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build mono consensus target");
        return;
    };

    eprintln!(
        "=== GHZ refined side lowmid candidate vs current === target_self_spectral={:.4} target_self_rms_fit={:.4}",
        target.self_spectral, target.self_rms_fit
    );
    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        let hybrid = score_fixed_hybrid_mono_consensus_candidate(
            name,
            rendered,
            &fixed_refs,
            &loaded_refs,
            &target,
        )
        .expect("expected hybrid score");
        let primary_ref = &loaded_refs[0].1;
        let selection = select_trusted_reference_window(rendered, primary_ref)
            .expect("expected trusted window");
        let fixed = score_fixed_trusted_window(rendered, primary_ref, selection)
            .expect("expected fixed trusted window metrics");
        eprintln!(
            "{name:>20}: hybrid_final={:.4} trusted_spectral={:.4} trusted_rms={:.4} trusted_mid={:.4} trusted_side={:.4}",
            hybrid.final_score, fixed.spectral, fixed.rms_ratio, fixed.mid, fixed.side
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_hybrid_mono_consensus_side_lowmid_q_sweep() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];
    let Some((target, fixed_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build mono consensus target");
        return;
    };

    let q_values = [
        0.70f32, 0.80, 0.90, 0.95, 1.00, 1.10, 1.20, 1.30, 1.40, 1.50, 1.60, 1.80,
    ];
    let mut results = Vec::new();
    eprintln!(
        "=== GHZ hybrid mono consensus side lowmid Q sweep === target_self_spectral={:.4} target_self_rms_fit={:.4}",
        target.self_spectral, target.self_rms_fit
    );
    for q in q_values {
        let config = current_default
            .with_post_side_eq_1(AudioEqStage::peaking(450.0, q, -4.0))
            .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 0.0));
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        let Some(score) = score_fixed_hybrid_mono_consensus_candidate(
            &format!("side_450_q{q:.2}"),
            rendered,
            &fixed_refs,
            &loaded_refs,
            &target,
        ) else {
            continue;
        };
        results.push((q, score));
    }
    results.sort_by(|a, b| {
        b.1.final_score
            .partial_cmp(&a.1.final_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.1.average_score
                    .partial_cmp(&a.1.average_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    for (q, score) in results {
        eprintln!(
            "side_lowmid_q q={q:.2}: average={:.4} penalty={:.4} final={:.4}",
            score.average_score, score.disagreement_penalty, score.final_score
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_refined_side_lowmid_q_candidate_vs_current() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let refined_q = current_default
        .with_post_side_eq_1(AudioEqStage::peaking(450.0, 1.50, -4.0))
        .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 0.0));
    let candidates = [
        ("current_default", current_default),
        ("refined_side_lowmid_q", refined_q),
    ];

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];
    let Some((target, fixed_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build mono consensus target");
        return;
    };

    eprintln!(
        "=== GHZ refined side lowmid Q candidate vs current === target_self_spectral={:.4} target_self_rms_fit={:.4}",
        target.self_spectral, target.self_rms_fit
    );
    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        let hybrid = score_fixed_hybrid_mono_consensus_candidate(
            name,
            rendered,
            &fixed_refs,
            &loaded_refs,
            &target,
        )
        .expect("expected hybrid score");
        let primary_ref = &loaded_refs[0].1;
        let selection = select_trusted_reference_window(rendered, primary_ref)
            .expect("expected trusted window");
        let fixed = score_fixed_trusted_window(rendered, primary_ref, selection)
            .expect("expected fixed trusted window metrics");
        eprintln!(
            "{name:>22}: hybrid_final={:.4} trusted_spectral={:.4} trusted_rms={:.4} trusted_mid={:.4} trusted_side={:.4}",
            hybrid.final_score, fixed.spectral, fixed.rms_ratio, fixed.mid, fixed.side
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_sectioned_side_consensus_candidates() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let candidates = [
        ("current_default", current_default),
        ("no_side_eq", calibrated_default_without_side_eq()),
        (
            "side_q_1_50",
            current_default
                .with_post_side_eq_1(AudioEqStage::peaking(450.0, 1.50, -4.0))
                .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 0.0)),
        ),
    ];

    let section_len = SAMPLE_RATE as usize;

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some(section_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, section_len)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let common_len = section_targets
        .iter()
        .map(|section| section.start + section.len)
        .max()
        .unwrap_or(0);

    /*
    let mut section_targets = Vec::new();
    for (start, len) in partition_window_sections(common_len, section_len) {
        if len < frame_len {
            continue;
        }

        let mut windows = Vec::new();
        let mut refs_meta = Vec::new();
        for (_name, ref_index, selection, base_weight) in &fixed_refs {
            let ref_samples = &loaded_refs[*ref_index].1;
            let Some(self_window) = reference_self_window_at_offset(
                ref_samples,
                selection.trusted_prior_start + start,
                selection.trusted_ref_start + start,
                len,
                frame_len,
                hop_len,
                &spectral_bins,
            ) else {
                continue;
            };
            let weight = *base_weight * side_section_consistency_weight(self_window);
            let Some(window) = extract_side_consensus_window(
                ref_samples,
                selection.trusted_ref_start + start,
                len,
                frame_len,
                hop_len,
                &spectral_bins,
                weight,
            ) else {
                continue;
            };
            windows.push(window);
            refs_meta.push(FixedSideSectionRef {
                emu_start: selection.trusted_emu_start + start,
                weight,
            });
        }

        if windows.len() < 2 {
            continue;
        }
        let Some(target) = build_side_consensus_target(&windows) else {
            continue;
        };
        let window_weights: Vec<f32> = windows.iter().map(|window| window.weight).collect();
        let uniform = vec![1.0; window_weights.len()];
        let weight = weighted_average(&window_weights, &uniform)
            * (target.self_spectral * 0.85 + target.self_ratio_fit * 0.15);
        section_targets.push(FixedSideSectionTarget {
            start,
            len,
            target,
            refs: refs_meta,
            weight,
        });
    }

    if section_targets.is_empty() {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    }
    */

    let mut weakest_sections: Vec<_> = section_targets
        .iter()
        .map(|section| {
            (
                section.start,
                section.len,
                section.weight,
                section.target.self_spectral,
                section.target.self_ratio_fit,
            )
        })
        .collect();
    weakest_sections.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));

    eprintln!(
        "=== GHZ sectioned side consensus candidates === sections={} common_len={:.2}s",
        section_targets.len(),
        common_len as f32 / SAMPLE_RATE as f32
    );
    for (start, len, weight, self_spectral, self_ratio_fit) in weakest_sections.into_iter().take(5)
    {
        eprintln!(
            "weak section {:.2}s..{:.2}s: weight={weight:.4} self_spectral={self_spectral:.4} self_ratio={self_ratio_fit:.4}",
            start as f32 / SAMPLE_RATE as f32,
            (start + len) as f32 / SAMPLE_RATE as f32,
        );
    }

    let mut scored = Vec::new();
    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];

        let Some(score) =
            score_fixed_sectioned_side_consensus_candidate(name, rendered, &section_targets)
        else {
            continue;
        };
        scored.push((
            name,
            score.average_score,
            score.disagreement_penalty,
            score.final_score,
            score.worst_section_score,
            score.dominant_section_impact,
        ));
    }

    scored.sort_by(|a, b| {
        b.3.partial_cmp(&a.3)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });
    for (name, average, penalty, final_score, worst_section, dominant_impact) in scored {
        eprintln!(
            "{name:>20}: average={average:.4} penalty={penalty:.4} final={final_score:.4} worst_section={worst_section:.4} dominant_impact={dominant_impact:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_sectioned_side_weak_sections() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let candidates = [
        ("current_default", current_default),
        (
            "side_q_1_50",
            current_default
                .with_post_side_eq_1(AudioEqStage::peaking(450.0, 1.50, -4.0))
                .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 0.0)),
        ),
        ("no_side_eq", calibrated_default_without_side_eq()),
    ];

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some(section_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let spectral_bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 12_000.0, 24);
    let format_deltas = |deltas: &[(f32, f32)]| -> String {
        deltas
            .iter()
            .map(|(hz, db)| format!("{hz:.0}Hz {db:+.2}dB"))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let mut analyzed_by_name = Vec::new();
    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        let Some(analyzed) = analyze_fixed_sectioned_side_consensus_candidate_sections(
            name,
            rendered,
            &section_targets,
        ) else {
            continue;
        };
        analyzed_by_name.push((name, analyzed));
    }
    if analyzed_by_name.is_empty() {
        eprintln!("No candidate side-section analyses available");
        return;
    }

    let current_sections = analyzed_by_name
        .iter()
        .find(|(name, _)| *name == "current_default")
        .map(|(_, sections)| sections)
        .expect("expected current_default analysis");
    let section_finals: Vec<f32> = current_sections
        .iter()
        .map(|section| section.final_score)
        .collect();
    let section_weights: Vec<f32> = section_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let dominant = section_impact_ranking(&section_finals, &section_weights);
    let raw_weakest = current_sections
        .iter()
        .enumerate()
        .min_by(|a, b| {
            a.1.final_score
                .partial_cmp(&b.1.final_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(idx, section)| (idx, section.final_score))
        .expect("expected raw weakest section");

    eprintln!("=== GHZ dominant weighted side sections ===");
    for (rank, &(idx, impact)) in dominant.iter().take(4).enumerate() {
        let target = &section_targets[idx];
        let reference_consistency =
            summarize_fixed_side_consensus_section_refs(target, &loaded_refs);
        eprintln!(
            "rank {} section {:.2}s..{:.2}s: target_weight={:.4} dominant_impact={:.4} target_self=({:.4}/{:.4}) target_ratio={:.4}",
            rank + 1,
            target.start as f32 / SAMPLE_RATE as f32,
            (target.start + target.len) as f32 / SAMPLE_RATE as f32,
            target.weight,
            impact,
            target.target.self_spectral,
            target.target.self_ratio_fit,
            target.target.ratio,
        );
        if let Some(reference_consistency) = reference_consistency {
            eprintln!(
                "  refs: pairs={} spectral={:.4} ratio_fit={:.4} worst={:.4}",
                reference_consistency.pair_count,
                reference_consistency.average_spectral_similarity,
                reference_consistency.average_ratio_fit,
                reference_consistency.worst_spectral_similarity,
            );
        }

        for (name, analyzed_sections) in &analyzed_by_name {
            let section = &analyzed_sections[idx];
            let deltas = largest_band_deltas_hz(
                &section.candidate.spectrum,
                &target.target.spectrum,
                2048,
                SAMPLE_RATE,
                &spectral_bins,
                5,
            );
            let ratio_delta = section.candidate.ratio - target.target.ratio;
            eprintln!(
                "  {name:>16}: final={:.4} avg={:.4} penalty={:.4} ratio={:.4} delta={:+.4} bands={}",
                section.final_score,
                section.average_score,
                section.disagreement_penalty,
                section.candidate.ratio,
                ratio_delta,
                format_deltas(&deltas),
            );
        }
    }

    let (raw_weakest_idx, raw_weakest_score) = raw_weakest;
    let raw_target = &section_targets[raw_weakest_idx];
    eprintln!(
        "raw weakest side section (secondary): {:.2}s..{:.2}s final={:.4} target_weight={:.4}",
        raw_target.start as f32 / SAMPLE_RATE as f32,
        (raw_target.start + raw_target.len) as f32 / SAMPLE_RATE as f32,
        raw_weakest_score,
        raw_target.weight,
    );
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_sectioned_side_dynamics() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let candidates = [
        ("current_default", current_default),
        (
            "side_q_1_50",
            current_default
                .with_post_side_eq_1(AudioEqStage::peaking(450.0, 1.50, -4.0))
                .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 0.0)),
        ),
        ("no_side_eq", calibrated_default_without_side_eq()),
    ];

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some(section_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let mut analyzed_by_name = Vec::new();
    let mut scored = Vec::new();
    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        let Some(score) =
            score_fixed_sectioned_side_dynamics_candidate(name, rendered, &section_targets)
        else {
            continue;
        };
        let Some(analyzed) = analyze_fixed_sectioned_side_dynamics_candidate_sections(
            name,
            rendered,
            &section_targets,
        ) else {
            continue;
        };
        analyzed_by_name.push((name, analyzed));
        scored.push((
            name,
            score.average_score,
            score.disagreement_penalty,
            score.final_score,
            score.worst_section_score,
            score.dominant_section_impact,
        ));
    }
    if analyzed_by_name.is_empty() {
        eprintln!("No candidate side-dynamics analyses available");
        return;
    }

    scored.sort_by(|a, b| {
        b.3.partial_cmp(&a.3)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ sectioned side dynamics ===");
    for (name, average, penalty, final_score, worst_section, dominant_impact) in &scored {
        eprintln!(
            "{name:>20}: average={average:.4} penalty={penalty:.4} final={final_score:.4} worst_section={worst_section:.4} dominant_impact={dominant_impact:.4}"
        );
    }

    let current_sections = analyzed_by_name
        .iter()
        .find(|(name, _)| *name == "current_default")
        .map(|(_, sections)| sections)
        .expect("expected current_default dynamics analysis");
    let section_finals: Vec<f32> = current_sections
        .iter()
        .map(|section| section.final_score)
        .collect();
    let section_weights: Vec<f32> = section_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let dominant = section_impact_ranking(&section_finals, &section_weights);
    let raw_weakest = current_sections
        .iter()
        .enumerate()
        .min_by(|a, b| {
            a.1.final_score
                .partial_cmp(&b.1.final_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(idx, section)| (idx, section.final_score))
        .expect("expected raw weakest dynamics section");

    eprintln!("=== GHZ dominant weighted side dynamics sections ===");
    for (rank, &(idx, impact)) in dominant.iter().take(4).enumerate() {
        let target = &section_targets[idx];
        let reference_consistency =
            summarize_fixed_side_dynamics_section_refs(target, &loaded_refs);
        eprintln!(
            "rank {} section {:.2}s..{:.2}s: target_weight={:.4} dominant_impact={:.4} target_self=({:.4}/{:.4}) target_ratio={:.4}",
            rank + 1,
            target.start as f32 / SAMPLE_RATE as f32,
            (target.start + target.len) as f32 / SAMPLE_RATE as f32,
            target.weight,
            impact,
            target.target.self_envelope,
            target.target.self_ratio_fit,
            target.target.ratio,
        );
        if let Some(reference_consistency) = reference_consistency {
            let lag_ms = reference_consistency.average_abs_lag_bins
                * SIDE_DYNAMICS_ENV_WINDOW as f32
                * 1000.0
                / SAMPLE_RATE as f32;
            eprintln!(
                "  refs: pairs={} env={:.4}->{:.4} trans={:.4}->{:.4} trans_rms={:.4} lag={:.1}ms worst_adj_trans={:.4}",
                reference_consistency.pair_count,
                reference_consistency.average_envelope_corr,
                reference_consistency.average_adjusted_envelope_corr,
                reference_consistency.average_transient_corr,
                reference_consistency.average_adjusted_transient_corr,
                reference_consistency.average_transient_rms_fit,
                lag_ms,
                reference_consistency.worst_adjusted_transient_corr,
            );
        }

        for (name, analyzed_sections) in &analyzed_by_name {
            let section = &analyzed_sections[idx];
            let envelope_fit =
                cross_correlation(&section.candidate.envelope, &target.target.envelope);
            let (offset_bins, adjusted_env) = best_envelope_offset_bins(
                &section.candidate.envelope,
                &target.target.envelope,
                SIDE_DYNAMICS_MAX_LAG_BINS,
            )
            .unwrap_or((0, envelope_fit));
            let ratio_delta = section.candidate.ratio - target.target.ratio;
            let lag_ms =
                offset_bins as f32 * SIDE_DYNAMICS_ENV_WINDOW as f32 * 1000.0 / SAMPLE_RATE as f32;
            let target_transient = envelope_transient_profile(&target.target.envelope);
            let candidate_transient = envelope_transient_profile(&section.candidate.envelope);
            let transient_fit = cross_correlation(&candidate_transient, &target_transient);
            let adjusted_transient =
                correlation_at_offset_bins(&candidate_transient, &target_transient, offset_bins);
            let transient_rms_ratio = rms(&candidate_transient) / rms(&target_transient).max(1e-9);
            eprintln!(
                "  {name:>16}: final={:.4} avg={:.4} penalty={:.4} env={envelope_fit:.4} lag={offset_bins:+} ({lag_ms:+.1}ms) adj_env={adjusted_env:.4} trans={transient_fit:.4} adj_trans={adjusted_transient:.4} trans_rms={transient_rms_ratio:.4} ratio={:.4} delta={:+.4}",
                section.final_score,
                section.average_score,
                section.disagreement_penalty,
                section.candidate.ratio,
                ratio_delta,
            );
        }
    }

    let (raw_weakest_idx, raw_weakest_score) = raw_weakest;
    let raw_target = &section_targets[raw_weakest_idx];
    eprintln!(
        "raw weakest side dynamics section (secondary): {:.2}s..{:.2}s final={:.4} target_weight={:.4}",
        raw_target.start as f32 / SAMPLE_RATE as f32,
        (raw_target.start + raw_target.len) as f32 / SAMPLE_RATE as f32,
        raw_weakest_score,
        raw_target.weight,
    );
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_dominant_section_chip_side_balance() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some(consensus_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };
    let current_consensus = analyze_fixed_sectioned_side_consensus_candidate_sections(
        "current_default",
        anchor_rendered,
        &consensus_targets,
    )
    .expect("expected current_default consensus analysis");
    let current_dynamics = analyze_fixed_sectioned_side_dynamics_candidate_sections(
        "current_default",
        anchor_rendered,
        &dynamics_targets,
    )
    .expect("expected current_default dynamics analysis");
    let consensus_finals: Vec<f32> = current_consensus
        .iter()
        .map(|section| section.final_score)
        .collect();
    let consensus_weights: Vec<f32> = consensus_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let dynamics_finals: Vec<f32> = current_dynamics
        .iter()
        .map(|section| section.final_score)
        .collect();
    let dynamics_weights: Vec<f32> = dynamics_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let consensus_idx = dominant_section_impact_index(&consensus_finals, &consensus_weights)
        .map(|(idx, _)| idx)
        .expect("expected dominant consensus section");
    let dynamics_idx = dominant_section_impact_index(&dynamics_finals, &dynamics_weights)
        .map(|(idx, _)| idx)
        .expect("expected dominant dynamics section");

    let candidates = [
        ("current_default", current_default),
        ("ym_only", current_default.with_psg_gain(0.0)),
        ("psg_only", current_default.with_ym_gain(0.0)),
    ];

    eprintln!("=== GHZ dominant section chip/side balance ===");
    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        let consensus = analyze_fixed_sectioned_side_consensus_candidate_sections(
            name,
            rendered,
            &consensus_targets,
        )
        .expect("expected consensus analysis");
        let dynamics = analyze_fixed_sectioned_side_dynamics_candidate_sections(
            name,
            rendered,
            &dynamics_targets,
        )
        .expect("expected dynamics analysis");

        let consensus_target = &consensus_targets[consensus_idx];
        let consensus_section = &consensus[consensus_idx];

        let dynamics_target = &dynamics_targets[dynamics_idx];
        let dynamics_section = &dynamics[dynamics_idx];
        let (offset_bins, adjusted_env) = best_envelope_offset_bins(
            &dynamics_section.candidate.envelope,
            &dynamics_target.target.envelope,
            SIDE_DYNAMICS_MAX_LAG_BINS,
        )
        .unwrap_or((0, 0.0));
        let candidate_transient = envelope_transient_profile(&dynamics_section.candidate.envelope);
        let target_transient = envelope_transient_profile(&dynamics_target.target.envelope);
        let adjusted_transient =
            correlation_at_offset_bins(&candidate_transient, &target_transient, offset_bins);

        eprintln!("-- {name} --");
        eprintln!(
            "  dominant side section {:.2}s..{:.2}s: final={:.4} ratio={:.4} target_ratio={:.4}",
            consensus_target.start as f32 / SAMPLE_RATE as f32,
            (consensus_target.start + consensus_target.len) as f32 / SAMPLE_RATE as f32,
            consensus_section.final_score,
            consensus_section.candidate.ratio,
            consensus_target.target.ratio,
        );
        eprintln!(
            "  dominant dynamics section {:.2}s..{:.2}s: final={:.4} ratio={:.4} target_ratio={:.4} adj_env={:.4} adj_trans={:.4}",
            dynamics_target.start as f32 / SAMPLE_RATE as f32,
            (dynamics_target.start + dynamics_target.len) as f32 / SAMPLE_RATE as f32,
            dynamics_section.final_score,
            dynamics_section.candidate.ratio,
            dynamics_target.target.ratio,
            adjusted_env,
            adjusted_transient,
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_dominant_pan_behavior_sweep() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];
    let Some((mono_target, fixed_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build mono consensus target");
        return;
    };
    let Some(consensus_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let current_consensus = analyze_fixed_sectioned_side_consensus_candidate_sections(
        "current_default",
        anchor_rendered,
        &consensus_targets,
    )
    .expect("expected current_default consensus analysis");
    let current_dynamics = analyze_fixed_sectioned_side_dynamics_candidate_sections(
        "current_default",
        anchor_rendered,
        &dynamics_targets,
    )
    .expect("expected current_default dynamics analysis");
    let consensus_finals: Vec<f32> = current_consensus
        .iter()
        .map(|section| section.final_score)
        .collect();
    let consensus_weights: Vec<f32> = consensus_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let dynamics_finals: Vec<f32> = current_dynamics
        .iter()
        .map(|section| section.final_score)
        .collect();
    let dynamics_weights: Vec<f32> = dynamics_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let (dominant_consensus_idx, dominant_consensus_impact) =
        dominant_section_impact_index(&consensus_finals, &consensus_weights)
            .expect("expected dominant consensus section");
    let (dominant_dynamics_idx, dominant_dynamics_impact) =
        dominant_section_impact_index(&dynamics_finals, &dynamics_weights)
            .expect("expected dominant dynamics section");

    let mut results = Vec::new();
    for mid_gain in [0.94f32, 0.97, 1.00, 1.03] {
        for side_gain in [0.95f32, 1.00, 1.05, 1.10, 1.15] {
            let config = current_default
                .with_mid_gain(mid_gain)
                .with_side_gain(side_gain);
            let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
            let rendered_full = renderer.render_timed_writes(
                &trace.ym_writes,
                &trace.psg_writes,
                0,
                trace.end_tick,
            );
            let capture_start = (trace.capture_start_sample as usize)
                .saturating_mul(2)
                .min(rendered_full.len());
            let rendered = &rendered_full[capture_start..];
            let Some(hybrid) = score_fixed_hybrid_mono_consensus_candidate(
                &format!("mid_{mid_gain:.2}_side_{side_gain:.2}"),
                rendered,
                &fixed_refs,
                &loaded_refs,
                &mono_target,
            ) else {
                continue;
            };
            let consensus = analyze_fixed_sectioned_side_consensus_candidate_sections(
                "candidate",
                rendered,
                &consensus_targets,
            )
            .expect("expected consensus analysis");
            let dynamics = analyze_fixed_sectioned_side_dynamics_candidate_sections(
                "candidate",
                rendered,
                &dynamics_targets,
            )
            .expect("expected dynamics analysis");
            let dominant_side_final = consensus[dominant_consensus_idx].final_score;
            let dominant_dynamics_final = dynamics[dominant_dynamics_idx].final_score;
            let combined = dominant_pan_behavior_score(
                hybrid.final_score,
                dominant_side_final,
                dominant_dynamics_final,
            );
            results.push((
                mid_gain,
                side_gain,
                hybrid.average_score,
                hybrid.disagreement_penalty,
                hybrid.final_score,
                dominant_side_final,
                dominant_dynamics_final,
                combined,
            ));
        }
    }

    results.sort_by(|a, b| {
        b.7.partial_cmp(&a.7)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.4.partial_cmp(&a.4).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.5.partial_cmp(&a.5).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.6.partial_cmp(&a.6).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!(
        "=== GHZ dominant pan behavior sweep === hybrid_target=({:.4}/{:.4}) side_idx={} impact={:.4} dyn_idx={} impact={:.4}",
        mono_target.self_spectral,
        mono_target.self_rms_fit,
        dominant_consensus_idx,
        dominant_consensus_impact,
        dominant_dynamics_idx,
        dominant_dynamics_impact,
    );
    for (
        mid_gain,
        side_gain,
        hybrid_average,
        hybrid_penalty,
        hybrid_final,
        dominant_side_final,
        dominant_dynamics_final,
        combined,
    ) in results.into_iter().take(12)
    {
        eprintln!(
            "mid={mid_gain:.2} side={side_gain:.2}: hybrid={hybrid_final:.4} ({hybrid_average:.4}-{hybrid_penalty:.4}) dom_side={dominant_side_final:.4} dom_dyn={dominant_dynamics_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_side_dynamics_primary_stereo_mix_refine() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let baseline_mono = score_fixed_mono_consensus_candidate(
        "current_default",
        anchor_rendered,
        &mono_refs,
        &mono_target,
    )
    .expect("expected current_default mono score");
    let baseline_side = score_fixed_sectioned_side_consensus_candidate(
        "current_default",
        anchor_rendered,
        &side_targets,
    )
    .expect("expected current_default side score");
    let baseline_dyn = score_fixed_sectioned_side_dynamics_candidate(
        "current_default",
        anchor_rendered,
        &dynamics_targets,
    )
    .expect("expected current_default dynamics score");

    let mono_guardrail = 0.9380f32;
    let mut results = Vec::new();
    for stereo_crossfeed in [0.15f32, 0.20, 0.25, 0.30] {
        for mid_gain in [0.91f32, 0.94, 0.97, 1.00] {
            for side_gain in [0.95f32, 1.00, 1.05, 1.10] {
                let config = current_default
                    .with_stereo_crossfeed(stereo_crossfeed)
                    .with_mid_gain(mid_gain)
                    .with_side_gain(side_gain);
                let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
                let rendered_full = renderer.render_timed_writes(
                    &trace.ym_writes,
                    &trace.psg_writes,
                    0,
                    trace.end_tick,
                );
                let capture_start = (trace.capture_start_sample as usize)
                    .saturating_mul(2)
                    .min(rendered_full.len());
                let rendered = &rendered_full[capture_start..];

                let Some(mono) = score_fixed_mono_consensus_candidate(
                    "candidate",
                    rendered,
                    &mono_refs,
                    &mono_target,
                ) else {
                    continue;
                };
                let Some(side_cons) = score_fixed_sectioned_side_consensus_candidate(
                    "candidate",
                    rendered,
                    &side_targets,
                ) else {
                    continue;
                };
                let Some(side_dyn) = score_fixed_sectioned_side_dynamics_candidate(
                    "candidate",
                    rendered,
                    &dynamics_targets,
                ) else {
                    continue;
                };

                results.push((
                    stereo_crossfeed,
                    mid_gain,
                    side_gain,
                    mono.final_score,
                    side_cons.final_score,
                    side_dyn.final_score,
                    mono.final_score >= mono_guardrail,
                    side_dyn.final_score - baseline_dyn.final_score,
                    side_cons.final_score - baseline_side.final_score,
                    mono.final_score - baseline_mono.final_score,
                ));
            }
        }
    }

    results.sort_by(|a, b| {
        b.6.cmp(&a.6)
            .then_with(|| b.5.partial_cmp(&a.5).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.4.partial_cmp(&a.4).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!(
        "=== GHZ side-dynamics-primary stereo mix refine === guardrail_mono={mono_guardrail:.4}"
    );
    eprintln!(
        "baseline: xf={:.2} mid={:.2} side={:.2} mono={:.4} side={:.4} dyn={:.4}",
        current_default.stereo_crossfeed,
        current_default.mid_gain,
        current_default.side_gain,
        baseline_mono.final_score,
        baseline_side.final_score,
        baseline_dyn.final_score,
    );
    for (
        stereo_crossfeed,
        mid_gain,
        side_gain,
        mono_final,
        side_final,
        dyn_final,
        passes_guardrail,
        dyn_delta,
        side_delta,
        mono_delta,
    ) in results.into_iter().take(16)
    {
        eprintln!(
            "xf={stereo_crossfeed:.2} mid={mid_gain:.2} side={side_gain:.2}: mono={mono_final:.4} ({mono_delta:+.4}) side={side_final:.4} ({side_delta:+.4}) dyn={dyn_final:.4} ({dyn_delta:+.4}) guardrail={passes_guardrail}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_side_dynamics_primary_side_eq_refine() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let baseline_mono = score_fixed_mono_consensus_candidate(
        "current_default",
        anchor_rendered,
        &mono_refs,
        &mono_target,
    )
    .expect("expected current_default mono score");
    let baseline_side = score_fixed_sectioned_side_consensus_candidate(
        "current_default",
        anchor_rendered,
        &side_targets,
    )
    .expect("expected current_default side score");
    let baseline_dyn = score_fixed_sectioned_side_dynamics_candidate(
        "current_default",
        anchor_rendered,
        &dynamics_targets,
    )
    .expect("expected current_default dynamics score");

    let mono_guardrail = 0.9380f32;
    let mut results = Vec::new();
    for freq_hz in [420.0f32, 450.0, 480.0] {
        for q in [0.85f32, 0.95, 1.05] {
            for cut_db in [-3.0f32, -3.5, -4.0] {
                let config = current_default
                    .with_post_side_eq_1(AudioEqStage::peaking(freq_hz, q, cut_db))
                    .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 0.0));
                let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
                let rendered_full = renderer.render_timed_writes(
                    &trace.ym_writes,
                    &trace.psg_writes,
                    0,
                    trace.end_tick,
                );
                let capture_start = (trace.capture_start_sample as usize)
                    .saturating_mul(2)
                    .min(rendered_full.len());
                let rendered = &rendered_full[capture_start..];

                let Some(mono) = score_fixed_mono_consensus_candidate(
                    "candidate",
                    rendered,
                    &mono_refs,
                    &mono_target,
                ) else {
                    continue;
                };
                let Some(side_cons) = score_fixed_sectioned_side_consensus_candidate(
                    "candidate",
                    rendered,
                    &side_targets,
                ) else {
                    continue;
                };
                let Some(side_dyn) = score_fixed_sectioned_side_dynamics_candidate(
                    "candidate",
                    rendered,
                    &dynamics_targets,
                ) else {
                    continue;
                };

                results.push((
                    freq_hz,
                    q,
                    cut_db,
                    mono.final_score,
                    side_cons.final_score,
                    side_dyn.final_score,
                    mono.final_score >= mono_guardrail,
                    side_dyn.final_score - baseline_dyn.final_score,
                    side_cons.final_score - baseline_side.final_score,
                    mono.final_score - baseline_mono.final_score,
                ));
            }
        }
    }

    results.sort_by(|a, b| {
        b.6.cmp(&a.6)
            .then_with(|| b.5.partial_cmp(&a.5).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.4.partial_cmp(&a.4).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!(
        "=== GHZ side-dynamics-primary side EQ refine === guardrail_mono={mono_guardrail:.4}"
    );
    eprintln!(
        "baseline: side_eq1=450Hz Q0.95 -4.0dB side_eq2=2600Hz Q0.90 0.0dB mono={:.4} side={:.4} dyn={:.4}",
        baseline_mono.final_score, baseline_side.final_score, baseline_dyn.final_score,
    );
    for (
        freq_hz,
        q,
        cut_db,
        mono_final,
        side_final,
        dyn_final,
        passes_guardrail,
        dyn_delta,
        side_delta,
        mono_delta,
    ) in results.into_iter().take(16)
    {
        eprintln!(
            "side_eq1={freq_hz:.0}Hz Q{q:.2} {cut_db:+.1}dB: mono={mono_final:.4} ({mono_delta:+.4}) side={side_final:.4} ({side_delta:+.4}) dyn={dyn_final:.4} ({dyn_delta:+.4}) guardrail={passes_guardrail}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_side_dynamics_primary_stereo_eq_combo_refine() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let baseline_mono = score_fixed_mono_consensus_candidate(
        "current_default",
        anchor_rendered,
        &mono_refs,
        &mono_target,
    )
    .expect("expected current_default mono score");
    let baseline_side = score_fixed_sectioned_side_consensus_candidate(
        "current_default",
        anchor_rendered,
        &side_targets,
    )
    .expect("expected current_default side score");
    let baseline_dyn = score_fixed_sectioned_side_dynamics_candidate(
        "current_default",
        anchor_rendered,
        &dynamics_targets,
    )
    .expect("expected current_default dynamics score");

    let mono_guardrail = 0.9380f32;
    let mut results = Vec::new();
    let side_eq_variants = [
        ("baseline", 450.0f32, 0.95f32, -4.0f32),
        ("420_q085_m4", 420.0f32, 0.85f32, -4.0f32),
        ("420_q095_m4", 420.0f32, 0.95f32, -4.0f32),
    ];
    for stereo_crossfeed in [0.25f32, 0.30] {
        for mid_gain in [0.91f32, 0.94] {
            for side_gain in [0.95f32, 1.00] {
                for (label, freq_hz, q, cut_db) in side_eq_variants {
                    let config = current_default
                        .with_stereo_crossfeed(stereo_crossfeed)
                        .with_mid_gain(mid_gain)
                        .with_side_gain(side_gain)
                        .with_post_side_eq_1(AudioEqStage::peaking(freq_hz, q, cut_db))
                        .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 0.0));
                    let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
                    let rendered_full = renderer.render_timed_writes(
                        &trace.ym_writes,
                        &trace.psg_writes,
                        0,
                        trace.end_tick,
                    );
                    let capture_start = (trace.capture_start_sample as usize)
                        .saturating_mul(2)
                        .min(rendered_full.len());
                    let rendered = &rendered_full[capture_start..];

                    let Some(mono) = score_fixed_mono_consensus_candidate(
                        "candidate",
                        rendered,
                        &mono_refs,
                        &mono_target,
                    ) else {
                        continue;
                    };
                    let Some(side_cons) = score_fixed_sectioned_side_consensus_candidate(
                        "candidate",
                        rendered,
                        &side_targets,
                    ) else {
                        continue;
                    };
                    let Some(side_dyn) = score_fixed_sectioned_side_dynamics_candidate(
                        "candidate",
                        rendered,
                        &dynamics_targets,
                    ) else {
                        continue;
                    };

                    results.push((
                        label,
                        stereo_crossfeed,
                        mid_gain,
                        side_gain,
                        mono.final_score,
                        side_cons.final_score,
                        side_dyn.final_score,
                        mono.final_score >= mono_guardrail,
                        side_dyn.final_score - baseline_dyn.final_score,
                        side_cons.final_score - baseline_side.final_score,
                        mono.final_score - baseline_mono.final_score,
                    ));
                }
            }
        }
    }

    results.sort_by(|a, b| {
        b.7.cmp(&a.7)
            .then_with(|| b.6.partial_cmp(&a.6).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.5.partial_cmp(&a.5).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.4.partial_cmp(&a.4).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!(
        "=== GHZ side-dynamics-primary stereo/EQ combo refine === guardrail_mono={mono_guardrail:.4}"
    );
    eprintln!(
        "baseline: xf={:.2} mid={:.2} side={:.2} side_eq1=450Hz Q0.95 -4.0dB mono={:.4} side={:.4} dyn={:.4}",
        current_default.stereo_crossfeed,
        current_default.mid_gain,
        current_default.side_gain,
        baseline_mono.final_score,
        baseline_side.final_score,
        baseline_dyn.final_score,
    );
    for (
        label,
        stereo_crossfeed,
        mid_gain,
        side_gain,
        mono_final,
        side_final,
        dyn_final,
        passes_guardrail,
        dyn_delta,
        side_delta,
        mono_delta,
    ) in results.into_iter().take(16)
    {
        eprintln!(
            "{label}: xf={stereo_crossfeed:.2} mid={mid_gain:.2} side={side_gain:.2} mono={mono_final:.4} ({mono_delta:+.4}) side={side_final:.4} ({side_delta:+.4}) dyn={dyn_final:.4} ({dyn_delta:+.4}) guardrail={passes_guardrail}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_pan_write_delay_candidates() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let ch4_mask = 1u8 << 3;
    let ch5_mask = 1u8 << 4;
    let ch45_mask = ch4_mask | ch5_mask;
    let delay_ms_values = [0.5f32, 1.0, 2.0, 5.0, 10.0, 20.0];
    let mut results = Vec::new();

    for delay_ms in delay_ms_values {
        let delay_ticks = ((delay_ms as f64 / 1000.0) * MASTER_CLOCK_NTSC as f64).round() as u64;
        for (name, channel_mask) in [("ch4", ch4_mask), ("ch5", ch5_mask), ("ch4_ch5", ch45_mask)] {
            let ym_writes =
                delay_pan_writes_for_channels(&trace.ym_writes, delay_ticks, channel_mask);
            let mut renderer = CoreAudioRenderer::with_audio_output_config(current_default);
            let rendered_full =
                renderer.render_timed_writes(&ym_writes, &trace.psg_writes, 0, trace.end_tick);
            let rendered = &rendered_full[capture_start..];
            let Some(mono) =
                score_fixed_mono_consensus_candidate(name, rendered, &mono_refs, &mono_target)
            else {
                continue;
            };
            let Some(side_cons) =
                score_fixed_sectioned_side_consensus_candidate(name, rendered, &side_targets)
            else {
                continue;
            };
            let Some(side_dyn) =
                score_fixed_sectioned_side_dynamics_candidate(name, rendered, &dynamics_targets)
            else {
                continue;
            };
            let combined = mono.final_score * 0.70
                + side_cons.final_score * 0.20
                + side_dyn.final_score * 0.10;
            results.push((
                format!("{name}_{delay_ms:.1}ms"),
                delay_ms,
                mono.final_score,
                side_cons.final_score,
                side_dyn.final_score,
                combined,
            ));
        }
    }

    results.push((
        "current_default".to_owned(),
        0.0,
        score_fixed_mono_consensus_candidate(
            "current_default",
            anchor_rendered,
            &mono_refs,
            &mono_target,
        )
        .expect("expected mono score")
        .final_score,
        score_fixed_sectioned_side_consensus_candidate(
            "current_default",
            anchor_rendered,
            &side_targets,
        )
        .expect("expected side score")
        .final_score,
        score_fixed_sectioned_side_dynamics_candidate(
            "current_default",
            anchor_rendered,
            &dynamics_targets,
        )
        .expect("expected dynamics score")
        .final_score,
        0.0,
    ));

    if let Some(last) = results.last_mut() {
        last.5 = last.2 * 0.70 + last.3 * 0.20 + last.4 * 0.10;
    }

    results.sort_by(|a, b| b.5.partial_cmp(&a.5).unwrap_or(std::cmp::Ordering::Equal));

    eprintln!("=== GHZ pan write delay candidates ===");
    for (name, delay_ms, mono_final, side_final, dyn_final, combined) in results {
        eprintln!(
            "{name}: delay_ms={delay_ms:.1} mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_pan_write_delay_refine() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let ch5_mask = 1u8 << 4;
    let ch45_mask = (1u8 << 3) | ch5_mask;
    let mut results = Vec::new();

    for delay_ms in [12.0f32, 15.0, 18.0, 20.0, 22.0, 25.0, 30.0] {
        let delay_ticks = ((delay_ms as f64 / 1000.0) * MASTER_CLOCK_NTSC as f64).round() as u64;
        for (name, channel_mask) in [("ch5", ch5_mask), ("ch4_ch5", ch45_mask)] {
            let ym_writes =
                delay_pan_writes_for_channels(&trace.ym_writes, delay_ticks, channel_mask);
            let mut renderer = CoreAudioRenderer::with_audio_output_config(current_default);
            let rendered_full =
                renderer.render_timed_writes(&ym_writes, &trace.psg_writes, 0, trace.end_tick);
            let rendered = &rendered_full[capture_start..];
            let Some(mono) =
                score_fixed_mono_consensus_candidate(name, rendered, &mono_refs, &mono_target)
            else {
                continue;
            };
            let Some(side_cons) =
                score_fixed_sectioned_side_consensus_candidate(name, rendered, &side_targets)
            else {
                continue;
            };
            let Some(side_dyn) =
                score_fixed_sectioned_side_dynamics_candidate(name, rendered, &dynamics_targets)
            else {
                continue;
            };
            let combined = mono.final_score * 0.70
                + side_cons.final_score * 0.20
                + side_dyn.final_score * 0.10;
            results.push((
                format!("{name}_{delay_ms:.1}ms"),
                delay_ms,
                mono.final_score,
                side_cons.final_score,
                side_dyn.final_score,
                combined,
            ));
        }
    }

    results.sort_by(|a, b| b.5.partial_cmp(&a.5).unwrap_or(std::cmp::Ordering::Equal));

    eprintln!("=== GHZ pan write delay refine ===");
    for (name, delay_ms, mono_final, side_final, dyn_final, combined) in results {
        eprintln!(
            "{name}: delay_ms={delay_ms:.1} mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_ym_pan_edge_persistence_candidates() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let candidates = [
        ("baseline", [0.0; 6], [0.0; 6]),
        (
            "ch5_a005_d10",
            [0.0, 0.0, 0.0, 0.0, 0.05, 0.0],
            [0.0, 0.0, 0.0, 0.0, 10.0, 0.0],
        ),
        (
            "ch5_a005_d20",
            [0.0, 0.0, 0.0, 0.0, 0.05, 0.0],
            [0.0, 0.0, 0.0, 0.0, 20.0, 0.0],
        ),
        (
            "ch5_a005_d25",
            [0.0, 0.0, 0.0, 0.0, 0.05, 0.0],
            [0.0, 0.0, 0.0, 0.0, 25.0, 0.0],
        ),
        (
            "ch5_a005_d30",
            [0.0, 0.0, 0.0, 0.0, 0.05, 0.0],
            [0.0, 0.0, 0.0, 0.0, 30.0, 0.0],
        ),
        (
            "ch5_a010_d10",
            [0.0, 0.0, 0.0, 0.0, 0.10, 0.0],
            [0.0, 0.0, 0.0, 0.0, 10.0, 0.0],
        ),
        (
            "ch5_a010_d20",
            [0.0, 0.0, 0.0, 0.0, 0.10, 0.0],
            [0.0, 0.0, 0.0, 0.0, 20.0, 0.0],
        ),
        (
            "ch5_a010_d25",
            [0.0, 0.0, 0.0, 0.0, 0.10, 0.0],
            [0.0, 0.0, 0.0, 0.0, 25.0, 0.0],
        ),
        (
            "ch5_a010_d30",
            [0.0, 0.0, 0.0, 0.0, 0.10, 0.0],
            [0.0, 0.0, 0.0, 0.0, 30.0, 0.0],
        ),
        (
            "ch5_a015_d20",
            [0.0, 0.0, 0.0, 0.0, 0.15, 0.0],
            [0.0, 0.0, 0.0, 0.0, 20.0, 0.0],
        ),
        (
            "ch5_a015_d25",
            [0.0, 0.0, 0.0, 0.0, 0.15, 0.0],
            [0.0, 0.0, 0.0, 0.0, 25.0, 0.0],
        ),
        (
            "ch4_a005_ch5_a010_d20",
            [0.0, 0.0, 0.0, 0.05, 0.10, 0.0],
            [0.0, 0.0, 0.0, 20.0, 20.0, 0.0],
        ),
        (
            "ch4_a005_ch5_a010_d25",
            [0.0, 0.0, 0.0, 0.05, 0.10, 0.0],
            [0.0, 0.0, 0.0, 25.0, 25.0, 0.0],
        ),
        (
            "ch4_a008_ch5_a012_d20",
            [0.0, 0.0, 0.0, 0.08, 0.12, 0.0],
            [0.0, 0.0, 0.0, 20.0, 20.0, 0.0],
        ),
        (
            "ch4_a008_ch5_a012_d25",
            [0.0, 0.0, 0.0, 0.08, 0.12, 0.0],
            [0.0, 0.0, 0.0, 25.0, 25.0, 0.0],
        ),
    ];
    let mut results = Vec::new();

    for (name, amounts, decay_ms) in candidates {
        let config = current_default
            .with_ym_channel_pan_edge_amounts(amounts)
            .with_ym_channel_pan_edge_decay_ms(decay_ms);
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];
        let Some(mono) =
            score_fixed_mono_consensus_candidate(name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(name, rendered, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name.to_owned(),
            amounts,
            decay_ms,
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.sort_by(|a, b| b.6.partial_cmp(&a.6).unwrap_or(std::cmp::Ordering::Equal));

    eprintln!("=== GHZ YM pan-edge persistence candidates ===");
    for (name, amounts, decay_ms, mono_final, side_final, dyn_final, combined) in results {
        eprintln!(
            "{name}: amounts={amounts:?} decay_ms={decay_ms:?} mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_pan_state_change_delay_candidates() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let ch4_mask = 1u8 << 3;
    let ch5_mask = 1u8 << 4;
    let ch45_mask = ch4_mask | ch5_mask;
    let candidates = [
        ("baseline", 0.0f32, 0u8),
        ("ch4_12ms", 12.0f32, ch4_mask),
        ("ch4_18ms", 18.0f32, ch4_mask),
        ("ch4_20ms", 20.0f32, ch4_mask),
        ("ch4_22ms", 22.0f32, ch4_mask),
        ("ch4_25ms", 25.0f32, ch4_mask),
        ("ch4_30ms", 30.0f32, ch4_mask),
        ("ch5_12ms", 12.0f32, ch5_mask),
        ("ch5_18ms", 18.0f32, ch5_mask),
        ("ch5_20ms", 20.0f32, ch5_mask),
        ("ch5_22ms", 22.0f32, ch5_mask),
        ("ch5_25ms", 25.0f32, ch5_mask),
        ("ch5_30ms", 30.0f32, ch5_mask),
        ("ch4_ch5_18ms", 18.0f32, ch45_mask),
        ("ch4_ch5_20ms", 20.0f32, ch45_mask),
        ("ch4_ch5_22ms", 22.0f32, ch45_mask),
        ("ch4_ch5_25ms", 25.0f32, ch45_mask),
        ("ch4_ch5_30ms", 30.0f32, ch45_mask),
    ];
    let mut results = Vec::new();

    for (name, delay_ms, channel_mask) in candidates {
        let rendered = if channel_mask == 0 {
            anchor_rendered.to_vec()
        } else {
            let delay_ticks =
                ((delay_ms as f64 / 1000.0) * MASTER_CLOCK_NTSC as f64).round() as u64;
            let ym_writes =
                delay_pan_state_changes_for_channels(&trace.ym_writes, delay_ticks, channel_mask);
            let mut renderer = CoreAudioRenderer::with_audio_output_config(current_default);
            let rendered_full =
                renderer.render_timed_writes(&ym_writes, &trace.psg_writes, 0, trace.end_tick);
            rendered_full[capture_start..].to_vec()
        };

        let Some(mono) =
            score_fixed_mono_consensus_candidate(name, &rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(name, &rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(name, &rendered, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name.to_owned(),
            delay_ms,
            channel_mask,
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.sort_by(|a, b| {
        b.5.partial_cmp(&a.5)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.4.partial_cmp(&a.4).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ pan state-change delay candidates ===");
    for (name, delay_ms, channel_mask, mono_final, side_final, dyn_final, combined) in results {
        eprintln!(
            "{name}: delay_ms={delay_ms:.1} mask=0x{channel_mask:02X} mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_dominant_ym_pan_states() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some(consensus_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let current_consensus = analyze_fixed_sectioned_side_consensus_candidate_sections(
        "current_default",
        anchor_rendered,
        &consensus_targets,
    )
    .expect("expected current_default consensus analysis");
    let current_dynamics = analyze_fixed_sectioned_side_dynamics_candidate_sections(
        "current_default",
        anchor_rendered,
        &dynamics_targets,
    )
    .expect("expected current_default dynamics analysis");

    let consensus_finals: Vec<f32> = current_consensus
        .iter()
        .map(|section| section.final_score)
        .collect();
    let consensus_weights: Vec<f32> = consensus_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let dynamics_finals: Vec<f32> = current_dynamics
        .iter()
        .map(|section| section.final_score)
        .collect();
    let dynamics_weights: Vec<f32> = dynamics_targets
        .iter()
        .map(|section| section.weight)
        .collect();

    let (consensus_idx, consensus_impact) =
        dominant_section_impact_index(&consensus_finals, &consensus_weights)
            .expect("expected dominant consensus section");
    let (dynamics_idx, dynamics_impact) =
        dominant_section_impact_index(&dynamics_finals, &dynamics_weights)
            .expect("expected dominant dynamics section");

    let dominant_sections = [
        (
            "side",
            &consensus_targets[consensus_idx].refs,
            consensus_targets[consensus_idx].start,
            consensus_targets[consensus_idx].len,
            current_consensus[consensus_idx].final_score,
            consensus_impact,
        ),
        (
            "dynamics",
            &dynamics_targets[dynamics_idx].refs,
            dynamics_targets[dynamics_idx].start,
            dynamics_targets[dynamics_idx].len,
            current_dynamics[dynamics_idx].final_score,
            dynamics_impact,
        ),
    ];

    eprintln!("=== GHZ dominant YM pan states ===");
    for (label, refs, start, len, final_score, impact) in dominant_sections {
        let (positions, start_tick, end_tick) = section_tick_range_from_refs(&trace, refs, len)
            .expect("expected dominant section absolute ranges");
        let total_ticks = end_tick.saturating_sub(start_tick);
        let summaries = summarize_ym_pan_states(&trace.ym_writes, start_tick, end_tick);

        eprintln!(
            "-- dominant {label} section local={:.2}s..{:.2}s abs=[{:.2}s,{:.2}s,{:.2}s..{:.2}s] final={final_score:.4} impact={impact:.4} ticks={total_ticks} --",
            start as f32 / SAMPLE_RATE as f32,
            (start + len) as f32 / SAMPLE_RATE as f32,
            positions.min_start as f32 / SAMPLE_RATE as f32,
            positions.mean_start as f32 / SAMPLE_RATE as f32,
            positions.max_start as f32 / SAMPLE_RATE as f32,
            positions.max_end as f32 / SAMPLE_RATE as f32,
        );
        for (idx, summary) in summaries.iter().enumerate() {
            eprintln!(
                "  ch{}: LR={:5.1}% L={:5.1}% R={:5.1}% off={:5.1}% changes={}",
                idx + 1,
                ratio_from_ticks(summary.stereo_ticks, total_ticks) * 100.0,
                ratio_from_ticks(summary.left_ticks, total_ticks) * 100.0,
                ratio_from_ticks(summary.right_ticks, total_ticks) * 100.0,
                ratio_from_ticks(summary.off_ticks, total_ticks) * 100.0,
                summary.change_count,
            );
        }
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_dominant_ym_pan_history() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some(consensus_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let current_consensus = analyze_fixed_sectioned_side_consensus_candidate_sections(
        "current_default",
        anchor_rendered,
        &consensus_targets,
    )
    .expect("expected current_default consensus analysis");
    let current_dynamics = analyze_fixed_sectioned_side_dynamics_candidate_sections(
        "current_default",
        anchor_rendered,
        &dynamics_targets,
    )
    .expect("expected current_default dynamics analysis");

    let consensus_finals: Vec<f32> = current_consensus
        .iter()
        .map(|section| section.final_score)
        .collect();
    let consensus_weights: Vec<f32> = consensus_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let dynamics_finals: Vec<f32> = current_dynamics
        .iter()
        .map(|section| section.final_score)
        .collect();
    let dynamics_weights: Vec<f32> = dynamics_targets
        .iter()
        .map(|section| section.weight)
        .collect();

    let (consensus_idx, consensus_impact) =
        dominant_section_impact_index(&consensus_finals, &consensus_weights)
            .expect("expected dominant consensus section");
    let (dynamics_idx, dynamics_impact) =
        dominant_section_impact_index(&dynamics_finals, &dynamics_weights)
            .expect("expected dominant dynamics section");

    let dominant_sections = [
        (
            "side",
            &consensus_targets[consensus_idx].refs,
            consensus_targets[consensus_idx].start,
            consensus_targets[consensus_idx].len,
            current_consensus[consensus_idx].final_score,
            consensus_impact,
        ),
        (
            "dynamics",
            &dynamics_targets[dynamics_idx].refs,
            dynamics_targets[dynamics_idx].start,
            dynamics_targets[dynamics_idx].len,
            current_dynamics[dynamics_idx].final_score,
            dynamics_impact,
        ),
    ];

    eprintln!("=== GHZ dominant YM pan history ===");
    for (label, refs, start, len, final_score, impact) in dominant_sections {
        let (positions, start_tick, _) = section_tick_range_from_refs(&trace, refs, len)
            .expect("expected dominant section absolute ranges");
        let last_changes = last_ym_pan_changes_before(&trace.ym_writes, start_tick);

        eprintln!(
            "-- dominant {label} section local={:.2}s..{:.2}s abs=[{:.2}s,{:.2}s,{:.2}s..{:.2}s] final={final_score:.4} impact={impact:.4} --",
            start as f32 / SAMPLE_RATE as f32,
            (start + len) as f32 / SAMPLE_RATE as f32,
            positions.min_start as f32 / SAMPLE_RATE as f32,
            positions.mean_start as f32 / SAMPLE_RATE as f32,
            positions.max_start as f32 / SAMPLE_RATE as f32,
            positions.max_end as f32 / SAMPLE_RATE as f32,
        );
        for (idx, change) in last_changes.iter().enumerate() {
            let (state, age_ms) = match change {
                Some((tick, state)) => {
                    let age_ms = ms_from_master_ticks(start_tick.saturating_sub(*tick));
                    (*state, Some(age_ms))
                }
                None => (YmPanState::Off, None),
            };
            let state_name = match state {
                YmPanState::Off => "off",
                YmPanState::Left => "L",
                YmPanState::Right => "R",
                YmPanState::Stereo => "LR",
            };
            match age_ms {
                Some(age_ms) => {
                    eprintln!(
                        "  ch{}: state={state_name:>3} last_change={age_ms:7.1}ms",
                        idx + 1
                    );
                }
                None => {
                    eprintln!(
                        "  ch{}: state={state_name:>3} last_change=   never",
                        idx + 1
                    );
                }
            }
        }
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_dominant_reference_side_authority() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some(consensus_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let current_consensus = analyze_fixed_sectioned_side_consensus_candidate_sections(
        "current_default",
        anchor_rendered,
        &consensus_targets,
    )
    .expect("expected current_default consensus analysis");
    let current_dynamics = analyze_fixed_sectioned_side_dynamics_candidate_sections(
        "current_default",
        anchor_rendered,
        &dynamics_targets,
    )
    .expect("expected current_default dynamics analysis");
    let consensus_finals: Vec<f32> = current_consensus
        .iter()
        .map(|section| section.final_score)
        .collect();
    let consensus_weights: Vec<f32> = consensus_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let dynamics_finals: Vec<f32> = current_dynamics
        .iter()
        .map(|section| section.final_score)
        .collect();
    let dynamics_weights: Vec<f32> = dynamics_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let (consensus_idx, _) = dominant_section_impact_index(&consensus_finals, &consensus_weights)
        .expect("expected dominant consensus section");
    let (dynamics_idx, _) = dominant_section_impact_index(&dynamics_finals, &dynamics_weights)
        .expect("expected dominant dynamics section");

    eprintln!("=== GHZ dominant reference side authority ===");
    for side_scale in [1.0f32, 0.85, 0.70, 0.55, 0.40, 0.25, 0.0] {
        let scaled_refs: Vec<_> = loaded_refs
            .iter()
            .map(|(path, samples)| (path.clone(), scale_side_channel(samples, side_scale)))
            .collect();
        let side_summary = summarize_fixed_side_consensus_section_refs(
            &consensus_targets[consensus_idx],
            &scaled_refs,
        )
        .expect("expected dominant side summary");
        let dynamics_summary = summarize_fixed_side_dynamics_section_refs(
            &dynamics_targets[dynamics_idx],
            &scaled_refs,
        )
        .expect("expected dominant dynamics summary");

        eprintln!(
            "side_scale={side_scale:.2}: dom_side spectral={:.4} ratio_fit={:.4} worst={:.4} | dom_dyn adj_env={:.4} adj_trans={:.4} rms_fit={:.4} lag_bins={:.2}",
            side_summary.average_spectral_similarity,
            side_summary.average_ratio_fit,
            side_summary.worst_spectral_similarity,
            dynamics_summary.average_adjusted_envelope_corr,
            dynamics_summary.average_adjusted_transient_corr,
            dynamics_summary.average_transient_rms_fit,
            dynamics_summary.average_abs_lag_bins,
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_centered_ym_side_leak() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let anchor_config = AudioOutputConfig::default();
    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(anchor_config);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some(consensus_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };
    let current_consensus = analyze_fixed_sectioned_side_consensus_candidate_sections(
        "current_default",
        anchor_rendered,
        &consensus_targets,
    )
    .expect("expected current_default consensus analysis");
    let current_dynamics = analyze_fixed_sectioned_side_dynamics_candidate_sections(
        "current_default",
        anchor_rendered,
        &dynamics_targets,
    )
    .expect("expected current_default dynamics analysis");
    let consensus_finals: Vec<f32> = current_consensus
        .iter()
        .map(|section| section.final_score)
        .collect();
    let consensus_weights: Vec<f32> = consensus_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let dynamics_finals: Vec<f32> = current_dynamics
        .iter()
        .map(|section| section.final_score)
        .collect();
    let dynamics_weights: Vec<f32> = dynamics_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let (consensus_idx, _) = dominant_section_impact_index(&consensus_finals, &consensus_weights)
        .expect("expected dominant consensus section");
    let (dynamics_idx, _) = dominant_section_impact_index(&dynamics_finals, &dynamics_weights)
        .expect("expected dominant dynamics section");

    let candidates = [
        (
            "current_default_ym_only",
            AudioOutputConfig::default().with_psg_gain(0.0),
        ),
        (
            "neutral_legacy_ym_only",
            AudioOutputConfig::legacy().with_psg_gain(0.0),
        ),
    ];

    eprintln!("=== GHZ centered YM side leak ===");
    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        let consensus = analyze_fixed_sectioned_side_consensus_candidate_sections(
            name,
            rendered,
            &consensus_targets,
        )
        .expect("expected consensus analysis");
        let dynamics = analyze_fixed_sectioned_side_dynamics_candidate_sections(
            name,
            rendered,
            &dynamics_targets,
        )
        .expect("expected dynamics analysis");

        let side_section = &consensus[consensus_idx];
        let dyn_section = &dynamics[dynamics_idx];
        eprintln!(
            "{name}: dom_side final={:.4} ratio={:.4} | dom_dyn final={:.4} ratio={:.4}",
            side_section.final_score,
            side_section.candidate.ratio,
            dyn_section.final_score,
            dyn_section.candidate.ratio,
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_forced_centered_pan_history() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let anchor_config = AudioOutputConfig::default();
    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(anchor_config);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some(consensus_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };
    let current_consensus = analyze_fixed_sectioned_side_consensus_candidate_sections(
        "current_default",
        anchor_rendered,
        &consensus_targets,
    )
    .expect("expected current_default consensus analysis");
    let current_dynamics = analyze_fixed_sectioned_side_dynamics_candidate_sections(
        "current_default",
        anchor_rendered,
        &dynamics_targets,
    )
    .expect("expected current_default dynamics analysis");
    let consensus_finals: Vec<f32> = current_consensus
        .iter()
        .map(|section| section.final_score)
        .collect();
    let consensus_weights: Vec<f32> = consensus_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let dynamics_finals: Vec<f32> = current_dynamics
        .iter()
        .map(|section| section.final_score)
        .collect();
    let dynamics_weights: Vec<f32> = dynamics_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let (consensus_idx, _) = dominant_section_impact_index(&consensus_finals, &consensus_weights)
        .expect("expected dominant consensus section");
    let (dynamics_idx, _) = dominant_section_impact_index(&dynamics_finals, &dynamics_weights)
        .expect("expected dominant dynamics section");

    let candidates = [
        ("original_trace", trace.ym_writes.clone()),
        (
            "pre_capture_pan_centered",
            force_centered_pan_writes_before(&trace.ym_writes, trace.capture_start_tick),
        ),
        (
            "all_pan_centered",
            force_centered_pan_writes(&trace.ym_writes),
        ),
    ];

    eprintln!("=== GHZ forced centered pan history ===");
    for (name, ym_writes) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(
            AudioOutputConfig::legacy().with_psg_gain(0.0),
        );
        let rendered_full =
            renderer.render_timed_writes(&ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        let consensus = analyze_fixed_sectioned_side_consensus_candidate_sections(
            name,
            rendered,
            &consensus_targets,
        )
        .expect("expected consensus analysis");
        let dynamics = analyze_fixed_sectioned_side_dynamics_candidate_sections(
            name,
            rendered,
            &dynamics_targets,
        )
        .expect("expected dynamics analysis");

        eprintln!(
            "{name}: dom_side final={:.4} ratio={:.4} | dom_dyn final={:.4} ratio={:.4}",
            consensus[consensus_idx].final_score,
            consensus[consensus_idx].candidate.ratio,
            dynamics[dynamics_idx].final_score,
            dynamics[dynamics_idx].candidate.ratio,
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_pan_history_horizon() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let anchor_config = AudioOutputConfig::default();
    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(anchor_config);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some(consensus_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };
    let current_consensus = analyze_fixed_sectioned_side_consensus_candidate_sections(
        "current_default",
        anchor_rendered,
        &consensus_targets,
    )
    .expect("expected current_default consensus analysis");
    let current_dynamics = analyze_fixed_sectioned_side_dynamics_candidate_sections(
        "current_default",
        anchor_rendered,
        &dynamics_targets,
    )
    .expect("expected current_default dynamics analysis");
    let consensus_finals: Vec<f32> = current_consensus
        .iter()
        .map(|section| section.final_score)
        .collect();
    let consensus_weights: Vec<f32> = consensus_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let dynamics_finals: Vec<f32> = current_dynamics
        .iter()
        .map(|section| section.final_score)
        .collect();
    let dynamics_weights: Vec<f32> = dynamics_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let (consensus_idx, _) = dominant_section_impact_index(&consensus_finals, &consensus_weights)
        .expect("expected dominant consensus section");
    let (dynamics_idx, _) = dominant_section_impact_index(&dynamics_finals, &dynamics_weights)
        .expect("expected dominant dynamics section");

    let consensus_pos = summarize_section_emu_positions(
        &consensus_targets[consensus_idx].refs,
        consensus_targets[consensus_idx].len,
    )
    .expect("expected dominant consensus emu positions");
    let dynamics_pos = summarize_section_emu_positions(
        &dynamics_targets[dynamics_idx].refs,
        dynamics_targets[dynamics_idx].len,
    )
    .expect("expected dominant dynamics emu positions");

    let mut cutoffs = vec![
        0usize,
        5 * SAMPLE_RATE as usize,
        consensus_pos.min_start,
        consensus_pos.mean_start,
        consensus_pos.max_end,
        dynamics_pos.min_start,
        dynamics_pos.mean_start,
        dynamics_pos.max_end,
    ];
    cutoffs.sort_unstable();
    cutoffs.dedup();

    eprintln!(
        "=== GHZ pan history horizon === side_abs=[{:.2}s,{:.2}s,{:.2}s..{:.2}s] dyn_abs=[{:.2}s,{:.2}s,{:.2}s..{:.2}s]",
        consensus_pos.min_start as f32 / SAMPLE_RATE as f32,
        consensus_pos.mean_start as f32 / SAMPLE_RATE as f32,
        consensus_pos.max_start as f32 / SAMPLE_RATE as f32,
        consensus_pos.max_end as f32 / SAMPLE_RATE as f32,
        dynamics_pos.min_start as f32 / SAMPLE_RATE as f32,
        dynamics_pos.mean_start as f32 / SAMPLE_RATE as f32,
        dynamics_pos.max_start as f32 / SAMPLE_RATE as f32,
        dynamics_pos.max_end as f32 / SAMPLE_RATE as f32,
    );
    for cutoff_samples in cutoffs {
        let cutoff_tick =
            trace.capture_start_tick + master_ticks_from_output_samples(cutoff_samples);
        let ym_writes = force_centered_pan_writes_before(&trace.ym_writes, cutoff_tick);
        let mut renderer = CoreAudioRenderer::with_audio_output_config(
            AudioOutputConfig::legacy().with_psg_gain(0.0),
        );
        let rendered_full =
            renderer.render_timed_writes(&ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        let consensus = analyze_fixed_sectioned_side_consensus_candidate_sections(
            "cutoff",
            rendered,
            &consensus_targets,
        )
        .expect("expected consensus analysis");
        let dynamics = analyze_fixed_sectioned_side_dynamics_candidate_sections(
            "cutoff",
            rendered,
            &dynamics_targets,
        )
        .expect("expected dynamics analysis");
        eprintln!(
            "cutoff_abs={:>7.2}s: dom_side final={:.4} ratio={:.4} | dom_dyn final={:.4} ratio={:.4}",
            cutoff_samples as f32 / SAMPLE_RATE as f32,
            consensus[consensus_idx].final_score,
            consensus[consensus_idx].candidate.ratio,
            dynamics[dynamics_idx].final_score,
            dynamics[dynamics_idx].candidate.ratio,
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_channel_pan_history_contributions() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let anchor_config = AudioOutputConfig::default();
    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(anchor_config);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some(consensus_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };
    let current_consensus = analyze_fixed_sectioned_side_consensus_candidate_sections(
        "current_default",
        anchor_rendered,
        &consensus_targets,
    )
    .expect("expected current_default consensus analysis");
    let current_dynamics = analyze_fixed_sectioned_side_dynamics_candidate_sections(
        "current_default",
        anchor_rendered,
        &dynamics_targets,
    )
    .expect("expected current_default dynamics analysis");
    let consensus_finals: Vec<f32> = current_consensus
        .iter()
        .map(|section| section.final_score)
        .collect();
    let consensus_weights: Vec<f32> = consensus_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let dynamics_finals: Vec<f32> = current_dynamics
        .iter()
        .map(|section| section.final_score)
        .collect();
    let dynamics_weights: Vec<f32> = dynamics_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let (consensus_idx, _) = dominant_section_impact_index(&consensus_finals, &consensus_weights)
        .expect("expected dominant consensus section");
    let (dynamics_idx, _) = dominant_section_impact_index(&dynamics_finals, &dynamics_weights)
        .expect("expected dominant dynamics section");

    let consensus_pos = summarize_section_emu_positions(
        &consensus_targets[consensus_idx].refs,
        consensus_targets[consensus_idx].len,
    )
    .expect("expected dominant consensus emu positions");
    let dynamics_pos = summarize_section_emu_positions(
        &dynamics_targets[dynamics_idx].refs,
        dynamics_targets[dynamics_idx].len,
    )
    .expect("expected dominant dynamics emu positions");

    let ch1_mask = 1u8 << 0;
    let ch4_mask = 1u8 << 3;
    let ch5_mask = 1u8 << 4;
    let ch45_mask = ch4_mask | ch5_mask;
    let ch145_mask = ch1_mask | ch45_mask;
    let dynamics_cutoff_tick =
        trace.capture_start_tick + master_ticks_from_output_samples(dynamics_pos.min_start);
    let side_cutoff_tick =
        trace.capture_start_tick + master_ticks_from_output_samples(consensus_pos.min_start);

    let candidates = [
        ("original_trace", trace.ym_writes.clone()),
        (
            "ch1_all_centered",
            force_centered_pan_writes_for_channels(&trace.ym_writes, ch1_mask),
        ),
        (
            "ch4_all_centered",
            force_centered_pan_writes_for_channels(&trace.ym_writes, ch4_mask),
        ),
        (
            "ch5_all_centered",
            force_centered_pan_writes_for_channels(&trace.ym_writes, ch5_mask),
        ),
        (
            "ch4_ch5_all_centered",
            force_centered_pan_writes_for_channels(&trace.ym_writes, ch45_mask),
        ),
        (
            "ch1_ch4_ch5_all_centered",
            force_centered_pan_writes_for_channels(&trace.ym_writes, ch145_mask),
        ),
        (
            "ch1_before_dyn_abs",
            force_centered_pan_writes_for_channels_before(
                &trace.ym_writes,
                dynamics_cutoff_tick,
                ch1_mask,
            ),
        ),
        (
            "ch4_before_dyn_abs",
            force_centered_pan_writes_for_channels_before(
                &trace.ym_writes,
                dynamics_cutoff_tick,
                ch4_mask,
            ),
        ),
        (
            "ch5_before_dyn_abs",
            force_centered_pan_writes_for_channels_before(
                &trace.ym_writes,
                dynamics_cutoff_tick,
                ch5_mask,
            ),
        ),
        (
            "ch4_ch5_before_dyn_abs",
            force_centered_pan_writes_for_channels_before(
                &trace.ym_writes,
                dynamics_cutoff_tick,
                ch45_mask,
            ),
        ),
        (
            "ch1_ch4_ch5_before_dyn_abs",
            force_centered_pan_writes_for_channels_before(
                &trace.ym_writes,
                dynamics_cutoff_tick,
                ch145_mask,
            ),
        ),
        (
            "ch1_before_side_abs",
            force_centered_pan_writes_for_channels_before(
                &trace.ym_writes,
                side_cutoff_tick,
                ch1_mask,
            ),
        ),
        (
            "ch4_before_side_abs",
            force_centered_pan_writes_for_channels_before(
                &trace.ym_writes,
                side_cutoff_tick,
                ch4_mask,
            ),
        ),
        (
            "ch5_before_side_abs",
            force_centered_pan_writes_for_channels_before(
                &trace.ym_writes,
                side_cutoff_tick,
                ch5_mask,
            ),
        ),
        (
            "ch4_ch5_before_side_abs",
            force_centered_pan_writes_for_channels_before(
                &trace.ym_writes,
                side_cutoff_tick,
                ch45_mask,
            ),
        ),
        (
            "ch1_ch4_ch5_before_side_abs",
            force_centered_pan_writes_for_channels_before(
                &trace.ym_writes,
                side_cutoff_tick,
                ch145_mask,
            ),
        ),
    ];

    eprintln!(
        "=== GHZ channel pan history contributions === dyn_abs={:.2}s side_abs={:.2}s",
        dynamics_pos.min_start as f32 / SAMPLE_RATE as f32,
        consensus_pos.min_start as f32 / SAMPLE_RATE as f32,
    );
    for (name, ym_writes) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(
            AudioOutputConfig::legacy().with_psg_gain(0.0),
        );
        let rendered_full =
            renderer.render_timed_writes(&ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        let consensus = analyze_fixed_sectioned_side_consensus_candidate_sections(
            name,
            rendered,
            &consensus_targets,
        )
        .expect("expected consensus analysis");
        let dynamics = analyze_fixed_sectioned_side_dynamics_candidate_sections(
            name,
            rendered,
            &dynamics_targets,
        )
        .expect("expected dynamics analysis");
        eprintln!(
            "{name}: dom_side final={:.4} ratio={:.4} | dom_dyn final={:.4} ratio={:.4}",
            consensus[consensus_idx].final_score,
            consensus[consensus_idx].candidate.ratio,
            dynamics[dynamics_idx].final_score,
            dynamics[dynamics_idx].candidate.ratio,
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_channel_stem_side_softening_sweep() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let stem_master_gain = 0.25f32;
    let final_stem_gain = current_default.master_gain / stem_master_gain;
    let linear_stem_config = current_default.with_gain(stem_master_gain);
    let ym_only_config = linear_stem_config.with_psg_gain(0.0);
    let psg_only_config = linear_stem_config.with_ym_gain(0.0);
    let ch1_mask = 1u8 << 0;
    let ch4_mask = 1u8 << 3;
    let ch5_mask = 1u8 << 4;
    let other_mask = 0x3Fu8 & !(ch1_mask | ch4_mask | ch5_mask);
    let render_ym_stem = |keep_mask: u8| {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(ym_only_config);
        let rendered_full = renderer.render_timed_writes(
            &mute_ym_pan_writes_outside_channels(&trace.ym_writes, keep_mask),
            &trace.psg_writes,
            0,
            trace.end_tick,
        );
        rendered_full[capture_start..].to_vec()
    };
    let ch1_stem = render_ym_stem(ch1_mask);
    let ch4_stem = render_ym_stem(ch4_mask);
    let ch5_stem = render_ym_stem(ch5_mask);
    let other_ym_stem = render_ym_stem(other_mask);
    let mut psg_renderer = CoreAudioRenderer::with_audio_output_config(psg_only_config);
    let psg_full =
        psg_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let psg_stem = psg_full[capture_start..].to_vec();

    let reconstructed_linear = sum_stereo_sources(&[
        ch1_stem.as_slice(),
        ch4_stem.as_slice(),
        ch5_stem.as_slice(),
        other_ym_stem.as_slice(),
        psg_stem.as_slice(),
    ]);
    let reconstructed = scale_and_clamp_stereo(&reconstructed_linear, final_stem_gain);
    let reconstruction_max_diff = anchor_rendered
        .iter()
        .zip(&reconstructed)
        .map(|(&a, &b)| (a - b).abs())
        .fold(0.0f32, f32::max);

    let candidates = [
        (1.00f32, 1.00f32, 1.00f32),
        (0.85f32, 1.00f32, 1.00f32),
        (0.70f32, 1.00f32, 1.00f32),
        (0.55f32, 1.00f32, 1.00f32),
        (1.00f32, 0.70f32, 1.00f32),
        (0.70f32, 0.85f32, 1.00f32),
        (0.70f32, 0.70f32, 1.00f32),
        (0.55f32, 0.85f32, 1.00f32),
        (0.55f32, 0.70f32, 1.00f32),
        (0.70f32, 1.00f32, 0.85f32),
        (0.70f32, 0.70f32, 0.85f32),
        (0.55f32, 0.70f32, 0.85f32),
    ];
    let mut results = Vec::new();

    for (ch1_scale, ch5_scale, ch4_scale) in candidates {
        let stem0 = scale_side_channel(&ch1_stem, ch1_scale);
        let stem3 = scale_side_channel(&ch4_stem, ch4_scale);
        let stem4 = scale_side_channel(&ch5_stem, ch5_scale);
        let candidate_linear = sum_stereo_sources(&[
            stem0.as_slice(),
            stem3.as_slice(),
            stem4.as_slice(),
            other_ym_stem.as_slice(),
            psg_stem.as_slice(),
        ]);
        let candidate = scale_and_clamp_stereo(&candidate_linear, final_stem_gain);
        let name = format!("ch1_{ch1_scale:.2}_ch5_{ch5_scale:.2}_ch4_{ch4_scale:.2}");
        let Some(mono) =
            score_fixed_mono_consensus_candidate(&name, &candidate, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(&name, &candidate, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(&name, &candidate, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name,
            ch1_scale,
            ch5_scale,
            ch4_scale,
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.sort_by(|a, b| b.7.partial_cmp(&a.7).unwrap_or(std::cmp::Ordering::Equal));

    eprintln!("=== GHZ channel stem side softening sweep ===");
    eprintln!("reconstruction_max_diff={reconstruction_max_diff:.8}");
    for (name, ch1_scale, ch5_scale, ch4_scale, mono_final, side_final, dyn_final, combined) in
        results.into_iter().take(16)
    {
        eprintln!(
            "{name}: ch1={ch1_scale:.2} ch5={ch5_scale:.2} ch4={ch4_scale:.2} mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_channel_stem_side_boost_sweep() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let stem_master_gain = 0.25f32;
    let final_stem_gain = current_default.master_gain / stem_master_gain;
    let linear_stem_config = current_default.with_gain(stem_master_gain);
    let ym_only_config = linear_stem_config.with_psg_gain(0.0);
    let psg_only_config = linear_stem_config.with_ym_gain(0.0);
    let ch1_mask = 1u8 << 0;
    let ch4_mask = 1u8 << 3;
    let ch5_mask = 1u8 << 4;
    let other_mask = 0x3Fu8 & !(ch1_mask | ch4_mask | ch5_mask);
    let render_ym_stem = |keep_mask: u8| {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(ym_only_config);
        let rendered_full = renderer.render_timed_writes(
            &mute_ym_pan_writes_outside_channels(&trace.ym_writes, keep_mask),
            &trace.psg_writes,
            0,
            trace.end_tick,
        );
        rendered_full[capture_start..].to_vec()
    };
    let ch1_stem = render_ym_stem(ch1_mask);
    let ch4_stem = render_ym_stem(ch4_mask);
    let ch5_stem = render_ym_stem(ch5_mask);
    let other_ym_stem = render_ym_stem(other_mask);
    let mut psg_renderer = CoreAudioRenderer::with_audio_output_config(psg_only_config);
    let psg_full =
        psg_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let psg_stem = psg_full[capture_start..].to_vec();

    let reconstructed_linear = sum_stereo_sources(&[
        ch1_stem.as_slice(),
        ch4_stem.as_slice(),
        ch5_stem.as_slice(),
        other_ym_stem.as_slice(),
        psg_stem.as_slice(),
    ]);
    let reconstructed = scale_and_clamp_stereo(&reconstructed_linear, final_stem_gain);
    let reconstruction_max_diff = anchor_rendered
        .iter()
        .zip(&reconstructed)
        .map(|(&a, &b)| (a - b).abs())
        .fold(0.0f32, f32::max);

    let candidates = [
        (1.00f32, 1.00f32, 1.00f32),
        (1.00f32, 1.10f32, 1.00f32),
        (1.00f32, 1.20f32, 1.00f32),
        (1.00f32, 1.30f32, 1.00f32),
        (1.00f32, 1.00f32, 1.10f32),
        (1.00f32, 1.00f32, 1.20f32),
        (1.00f32, 1.00f32, 1.30f32),
        (1.00f32, 1.10f32, 1.10f32),
        (1.00f32, 1.20f32, 1.10f32),
        (1.00f32, 1.10f32, 1.20f32),
        (1.00f32, 1.20f32, 1.20f32),
        (1.00f32, 1.30f32, 1.20f32),
        (1.00f32, 1.20f32, 1.30f32),
    ];
    let mut results = Vec::new();

    for (ch1_scale, ch5_scale, ch4_scale) in candidates {
        let stem0 = scale_side_channel(&ch1_stem, ch1_scale);
        let stem3 = scale_side_channel(&ch4_stem, ch4_scale);
        let stem4 = scale_side_channel(&ch5_stem, ch5_scale);
        let candidate_linear = sum_stereo_sources(&[
            stem0.as_slice(),
            stem3.as_slice(),
            stem4.as_slice(),
            other_ym_stem.as_slice(),
            psg_stem.as_slice(),
        ]);
        let candidate = scale_and_clamp_stereo(&candidate_linear, final_stem_gain);
        let name = format!("ch1_{ch1_scale:.2}_ch5_{ch5_scale:.2}_ch4_{ch4_scale:.2}");
        let Some(mono) =
            score_fixed_mono_consensus_candidate(&name, &candidate, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(&name, &candidate, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(&name, &candidate, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name,
            ch1_scale,
            ch5_scale,
            ch4_scale,
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.sort_by(|a, b| {
        b.6.partial_cmp(&a.6)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.5.partial_cmp(&a.5).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.4.partial_cmp(&a.4).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ channel stem side boost sweep ===");
    eprintln!("reconstruction_max_diff={reconstruction_max_diff:.8}");
    for (name, ch1_scale, ch5_scale, ch4_scale, mono_final, side_final, dyn_final, combined) in
        results.into_iter().take(16)
    {
        eprintln!(
            "{name}: ch1={ch1_scale:.2} ch5={ch5_scale:.2} ch4={ch4_scale:.2} mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_channel_stem_side_transient_sweep() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let stem_master_gain = 0.25f32;
    let final_stem_gain = current_default.master_gain / stem_master_gain;
    let linear_stem_config = current_default.with_gain(stem_master_gain);
    let ym_only_config = linear_stem_config.with_psg_gain(0.0);
    let psg_only_config = linear_stem_config.with_ym_gain(0.0);
    let ch1_mask = 1u8 << 0;
    let ch4_mask = 1u8 << 3;
    let ch5_mask = 1u8 << 4;
    let other_mask = 0x3Fu8 & !(ch1_mask | ch4_mask | ch5_mask);
    let render_ym_stem = |keep_mask: u8| {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(ym_only_config);
        let rendered_full = renderer.render_timed_writes(
            &mute_ym_pan_writes_outside_channels(&trace.ym_writes, keep_mask),
            &trace.psg_writes,
            0,
            trace.end_tick,
        );
        rendered_full[capture_start..].to_vec()
    };
    let ch1_stem = render_ym_stem(ch1_mask);
    let ch4_stem = render_ym_stem(ch4_mask);
    let ch5_stem = render_ym_stem(ch5_mask);
    let other_ym_stem = render_ym_stem(other_mask);
    let mut psg_renderer = CoreAudioRenderer::with_audio_output_config(psg_only_config);
    let psg_full =
        psg_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let psg_stem = psg_full[capture_start..].to_vec();

    let reconstructed_linear = sum_stereo_sources(&[
        ch1_stem.as_slice(),
        ch4_stem.as_slice(),
        ch5_stem.as_slice(),
        other_ym_stem.as_slice(),
        psg_stem.as_slice(),
    ]);
    let reconstructed = scale_and_clamp_stereo(&reconstructed_linear, final_stem_gain);
    let reconstruction_max_diff = anchor_rendered
        .iter()
        .zip(&reconstructed)
        .map(|(&a, &b)| (a - b).abs())
        .fold(0.0f32, f32::max);

    let candidates = [
        ("baseline", 0.00f32, 0.00f32, 0.00f32),
        ("ch5_t020", 0.00f32, 0.20f32, 0.00f32),
        ("ch5_t035", 0.00f32, 0.35f32, 0.00f32),
        ("ch4_t020", 0.00f32, 0.00f32, 0.20f32),
        ("ch4_t035", 0.00f32, 0.00f32, 0.35f32),
        ("ch1_t020", 0.20f32, 0.00f32, 0.00f32),
        ("ch5_t020_ch4_t020", 0.00f32, 0.20f32, 0.20f32),
        ("ch5_t035_ch4_t020", 0.00f32, 0.35f32, 0.20f32),
    ];
    let mut results = Vec::new();

    for (name, ch1_transient, ch5_transient, ch4_transient) in candidates {
        let stem0 = apply_side_transient_mix(&ch1_stem, ch1_transient);
        let stem3 = apply_side_transient_mix(&ch4_stem, ch4_transient);
        let stem4 = apply_side_transient_mix(&ch5_stem, ch5_transient);
        let candidate_linear = sum_stereo_sources(&[
            stem0.as_slice(),
            stem3.as_slice(),
            stem4.as_slice(),
            other_ym_stem.as_slice(),
            psg_stem.as_slice(),
        ]);
        let candidate = scale_and_clamp_stereo(&candidate_linear, final_stem_gain);
        let Some(mono) =
            score_fixed_mono_consensus_candidate(name, &candidate, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(name, &candidate, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(name, &candidate, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name.to_owned(),
            ch1_transient,
            ch5_transient,
            ch4_transient,
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.sort_by(|a, b| b.7.partial_cmp(&a.7).unwrap_or(std::cmp::Ordering::Equal));

    eprintln!("=== GHZ channel stem side transient sweep ===");
    eprintln!("reconstruction_max_diff={reconstruction_max_diff:.8}");
    for (
        name,
        ch1_transient,
        ch5_transient,
        ch4_transient,
        mono_final,
        side_final,
        dyn_final,
        combined,
    ) in results
    {
        eprintln!(
            "{name}: ch1_t={ch1_transient:.2} ch5_t={ch5_transient:.2} ch4_t={ch4_transient:.2} mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_channel_stem_key_persistence_sweep() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let stem_master_gain = 0.25f32;
    let final_stem_gain = current_default.master_gain / stem_master_gain;
    let linear_stem_config = current_default.with_gain(stem_master_gain);
    let ym_only_config = linear_stem_config.with_psg_gain(0.0);
    let psg_only_config = linear_stem_config.with_ym_gain(0.0);
    let ch1_mask = 1u8 << 0;
    let ch4_mask = 1u8 << 3;
    let ch5_mask = 1u8 << 4;
    let other_mask = 0x3Fu8 & !(ch1_mask | ch4_mask | ch5_mask);
    let render_ym_stem = |keep_mask: u8| {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(ym_only_config);
        let rendered_full = renderer.render_timed_writes(
            &mute_ym_pan_writes_outside_channels(&trace.ym_writes, keep_mask),
            &trace.psg_writes,
            0,
            trace.end_tick,
        );
        rendered_full[capture_start..].to_vec()
    };
    let ch1_stem = render_ym_stem(ch1_mask);
    let ch4_stem = render_ym_stem(ch4_mask);
    let ch5_stem = render_ym_stem(ch5_mask);
    let other_ym_stem = render_ym_stem(other_mask);
    let mut psg_renderer = CoreAudioRenderer::with_audio_output_config(psg_only_config);
    let psg_full =
        psg_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let psg_stem = psg_full[capture_start..].to_vec();

    let reconstructed_linear = sum_stereo_sources(&[
        ch1_stem.as_slice(),
        ch4_stem.as_slice(),
        ch5_stem.as_slice(),
        other_ym_stem.as_slice(),
        psg_stem.as_slice(),
    ]);
    let reconstructed = scale_and_clamp_stereo(&reconstructed_linear, final_stem_gain);
    let reconstruction_max_diff = anchor_rendered
        .iter()
        .zip(&reconstructed)
        .map(|(&a, &b)| (a - b).abs())
        .fold(0.0f32, f32::max);

    let capture_start_sample = trace.capture_start_sample as u64;
    let ch1_triggers = key_on_trigger_samples(&trace.ym_writes, capture_start_sample, ch1_mask);
    let ch4_triggers = key_on_trigger_samples(&trace.ym_writes, capture_start_sample, ch4_mask);
    let ch5_triggers = key_on_trigger_samples(&trace.ym_writes, capture_start_sample, ch5_mask);
    let ms_to_samples = |ms: f32| ((SAMPLE_RATE as f32 * ms / 1000.0).round() as usize).max(1);

    let candidates = [
        ("baseline", 0.0f32, 0usize, 0.0f32, 0usize, 0.0f32, 0usize),
        (
            "ch1_k015_d4ms",
            0.15f32,
            ms_to_samples(4.0),
            0.0f32,
            0usize,
            0.0f32,
            0usize,
        ),
        (
            "ch1_k020_d6ms",
            0.20f32,
            ms_to_samples(6.0),
            0.0f32,
            0usize,
            0.0f32,
            0usize,
        ),
        (
            "ch4_k020_d4ms",
            0.0f32,
            0usize,
            0.0f32,
            0usize,
            0.20f32,
            ms_to_samples(4.0),
        ),
        (
            "ch5_k020_d6ms",
            0.0f32,
            0usize,
            0.20f32,
            ms_to_samples(6.0),
            0.0f32,
            0usize,
        ),
        (
            "ch1_k015_d4ms_ch4_k015_d4ms",
            0.15f32,
            ms_to_samples(4.0),
            0.0f32,
            0usize,
            0.15f32,
            ms_to_samples(4.0),
        ),
        (
            "ch1_k015_d4ms_ch5_k020_d6ms",
            0.15f32,
            ms_to_samples(4.0),
            0.20f32,
            ms_to_samples(6.0),
            0.0f32,
            0usize,
        ),
        (
            "ch4_k020_d4ms_ch5_k020_d6ms",
            0.0f32,
            0usize,
            0.20f32,
            ms_to_samples(6.0),
            0.20f32,
            ms_to_samples(4.0),
        ),
        (
            "ch1_k015_d4ms_ch4_k015_d4ms_ch5_k020_d6ms",
            0.15f32,
            ms_to_samples(4.0),
            0.20f32,
            ms_to_samples(6.0),
            0.15f32,
            ms_to_samples(4.0),
        ),
        (
            "ch1_k020_d8ms_ch4_k020_d5ms_ch5_k025_d8ms",
            0.20f32,
            ms_to_samples(8.0),
            0.25f32,
            ms_to_samples(8.0),
            0.20f32,
            ms_to_samples(5.0),
        ),
    ];
    let mut results = Vec::new();

    for (name, ch1_amount, ch1_decay, ch5_amount, ch5_decay, ch4_amount, ch4_decay) in candidates {
        let stem0 =
            apply_side_triggered_persistence_mix(&ch1_stem, &ch1_triggers, ch1_amount, ch1_decay);
        let stem3 =
            apply_side_triggered_persistence_mix(&ch4_stem, &ch4_triggers, ch4_amount, ch4_decay);
        let stem4 =
            apply_side_triggered_persistence_mix(&ch5_stem, &ch5_triggers, ch5_amount, ch5_decay);
        let candidate_linear = sum_stereo_sources(&[
            stem0.as_slice(),
            stem3.as_slice(),
            stem4.as_slice(),
            other_ym_stem.as_slice(),
            psg_stem.as_slice(),
        ]);
        let candidate = scale_and_clamp_stereo(&candidate_linear, final_stem_gain);
        let Some(mono) =
            score_fixed_mono_consensus_candidate(name, &candidate, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(name, &candidate, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(name, &candidate, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name.to_owned(),
            ch1_amount,
            ch1_decay,
            ch5_amount,
            ch5_decay,
            ch4_amount,
            ch4_decay,
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.sort_by(|a, b| {
        b.9.partial_cmp(&a.9)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.7.partial_cmp(&a.7).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ channel stem key-persistence sweep ===");
    eprintln!(
        "reconstruction_max_diff={reconstruction_max_diff:.8} key_on_counts: ch1={} ch4={} ch5={}",
        ch1_triggers.len(),
        ch4_triggers.len(),
        ch5_triggers.len()
    );
    for (
        name,
        ch1_amount,
        ch1_decay,
        ch5_amount,
        ch5_decay,
        ch4_amount,
        ch4_decay,
        mono_final,
        side_final,
        dyn_final,
        combined,
    ) in results
    {
        eprintln!(
            "{name}: ch1_k={ch1_amount:.2}/{ch1_decay} ch5_k={ch5_amount:.2}/{ch5_decay} ch4_k={ch4_amount:.2}/{ch4_decay} mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_channel_stem_key_delayed_persistence_sweep() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let stem_master_gain = 0.25f32;
    let final_stem_gain = current_default.master_gain / stem_master_gain;
    let linear_stem_config = current_default.with_gain(stem_master_gain);
    let ym_only_config = linear_stem_config.with_psg_gain(0.0);
    let psg_only_config = linear_stem_config.with_ym_gain(0.0);
    let ch1_mask = 1u8 << 0;
    let ch4_mask = 1u8 << 3;
    let ch5_mask = 1u8 << 4;
    let other_mask = 0x3Fu8 & !(ch1_mask | ch4_mask | ch5_mask);
    let render_ym_stem = |keep_mask: u8| {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(ym_only_config);
        let rendered_full = renderer.render_timed_writes(
            &mute_ym_pan_writes_outside_channels(&trace.ym_writes, keep_mask),
            &trace.psg_writes,
            0,
            trace.end_tick,
        );
        rendered_full[capture_start..].to_vec()
    };
    let ch1_stem = render_ym_stem(ch1_mask);
    let ch4_stem = render_ym_stem(ch4_mask);
    let ch5_stem = render_ym_stem(ch5_mask);
    let other_ym_stem = render_ym_stem(other_mask);
    let mut psg_renderer = CoreAudioRenderer::with_audio_output_config(psg_only_config);
    let psg_full =
        psg_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let psg_stem = psg_full[capture_start..].to_vec();

    let capture_start_sample = trace.capture_start_sample as u64;
    let ch1_triggers = key_on_trigger_samples(&trace.ym_writes, capture_start_sample, ch1_mask);
    let ch4_triggers = key_on_trigger_samples(&trace.ym_writes, capture_start_sample, ch4_mask);
    let ch5_triggers = key_on_trigger_samples(&trace.ym_writes, capture_start_sample, ch5_mask);
    let ms_to_samples = |ms: f32| ((SAMPLE_RATE as f32 * ms / 1000.0).round() as usize).max(1);

    let candidates = [
        (
            "baseline", 0.0f32, 0usize, 0.0f32, 0usize, 0.0f32, 0usize, 0usize,
        ),
        (
            "ch1_dseed2ms_a025_decay6ms",
            0.25f32,
            ms_to_samples(2.0),
            0.0f32,
            0usize,
            0.0f32,
            0usize,
            ms_to_samples(6.0),
        ),
        (
            "ch4_dseed2ms_a020_decay5ms",
            0.0f32,
            0usize,
            0.0f32,
            0usize,
            0.20f32,
            ms_to_samples(2.0),
            ms_to_samples(5.0),
        ),
        (
            "ch5_dseed3ms_a025_decay8ms",
            0.0f32,
            0usize,
            0.25f32,
            ms_to_samples(3.0),
            0.0f32,
            0usize,
            ms_to_samples(8.0),
        ),
        (
            "ch1_dseed2ms_ch5_dseed3ms",
            0.25f32,
            ms_to_samples(2.0),
            0.25f32,
            ms_to_samples(3.0),
            0.0f32,
            0usize,
            ms_to_samples(8.0),
        ),
        (
            "ch4_dseed2ms_ch5_dseed3ms",
            0.0f32,
            0usize,
            0.25f32,
            ms_to_samples(3.0),
            0.20f32,
            ms_to_samples(2.0),
            ms_to_samples(8.0),
        ),
        (
            "ch1_dseed2ms_ch4_dseed2ms_ch5_dseed3ms",
            0.25f32,
            ms_to_samples(2.0),
            0.25f32,
            ms_to_samples(3.0),
            0.20f32,
            ms_to_samples(2.0),
            ms_to_samples(8.0),
        ),
        (
            "ch1_dseed4ms_ch4_dseed3ms_ch5_dseed5ms",
            0.30f32,
            ms_to_samples(4.0),
            0.30f32,
            ms_to_samples(5.0),
            0.22f32,
            ms_to_samples(3.0),
            ms_to_samples(10.0),
        ),
    ];
    let mut results = Vec::new();

    for (name, ch1_amount, ch1_delay, ch5_amount, ch5_delay, ch4_amount, ch4_delay, decay) in
        candidates
    {
        let stem0 = apply_side_triggered_delayed_persistence_mix(
            &ch1_stem,
            &ch1_triggers,
            ch1_delay,
            ch1_amount,
            decay,
        );
        let stem3 = apply_side_triggered_delayed_persistence_mix(
            &ch4_stem,
            &ch4_triggers,
            ch4_delay,
            ch4_amount,
            decay,
        );
        let stem4 = apply_side_triggered_delayed_persistence_mix(
            &ch5_stem,
            &ch5_triggers,
            ch5_delay,
            ch5_amount,
            decay,
        );
        let candidate_linear = sum_stereo_sources(&[
            stem0.as_slice(),
            stem3.as_slice(),
            stem4.as_slice(),
            other_ym_stem.as_slice(),
            psg_stem.as_slice(),
        ]);
        let candidate = scale_and_clamp_stereo(&candidate_linear, final_stem_gain);
        let Some(mono) =
            score_fixed_mono_consensus_candidate(name, &candidate, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(name, &candidate, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(name, &candidate, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name.to_owned(),
            ch1_amount,
            ch1_delay,
            ch5_amount,
            ch5_delay,
            ch4_amount,
            ch4_delay,
            decay,
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.sort_by(|a, b| {
        b.10.partial_cmp(&a.10)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.8.partial_cmp(&a.8).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ channel stem delayed key-persistence sweep ===");
    eprintln!(
        "key_on_counts: ch1={} ch4={} ch5={}",
        ch1_triggers.len(),
        ch4_triggers.len(),
        ch5_triggers.len()
    );
    for (
        name,
        ch1_amount,
        ch1_delay,
        ch5_amount,
        ch5_delay,
        ch4_amount,
        ch4_delay,
        decay,
        mono_final,
        side_final,
        dyn_final,
        combined,
    ) in results
    {
        eprintln!(
            "{name}: ch1_k={ch1_amount:.2}/{ch1_delay} ch5_k={ch5_amount:.2}/{ch5_delay} ch4_k={ch4_amount:.2}/{ch4_delay} decay={decay} mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_channel_stem_key_triggered_transient_sweep() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let stem_master_gain = 0.25f32;
    let final_stem_gain = current_default.master_gain / stem_master_gain;
    let linear_stem_config = current_default.with_gain(stem_master_gain);
    let ym_only_config = linear_stem_config.with_psg_gain(0.0);
    let psg_only_config = linear_stem_config.with_ym_gain(0.0);
    let ch1_mask = 1u8 << 0;
    let ch4_mask = 1u8 << 3;
    let ch5_mask = 1u8 << 4;
    let other_mask = 0x3Fu8 & !(ch1_mask | ch4_mask | ch5_mask);
    let render_ym_stem = |keep_mask: u8| {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(ym_only_config);
        let rendered_full = renderer.render_timed_writes(
            &mute_ym_pan_writes_outside_channels(&trace.ym_writes, keep_mask),
            &trace.psg_writes,
            0,
            trace.end_tick,
        );
        rendered_full[capture_start..].to_vec()
    };
    let ch1_stem = render_ym_stem(ch1_mask);
    let ch4_stem = render_ym_stem(ch4_mask);
    let ch5_stem = render_ym_stem(ch5_mask);
    let other_ym_stem = render_ym_stem(other_mask);
    let mut psg_renderer = CoreAudioRenderer::with_audio_output_config(psg_only_config);
    let psg_full =
        psg_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let psg_stem = psg_full[capture_start..].to_vec();

    let capture_start_sample = trace.capture_start_sample as u64;
    let ch1_triggers = key_on_trigger_samples(&trace.ym_writes, capture_start_sample, ch1_mask);
    let ch4_triggers = key_on_trigger_samples(&trace.ym_writes, capture_start_sample, ch4_mask);
    let ch5_triggers = key_on_trigger_samples(&trace.ym_writes, capture_start_sample, ch5_mask);
    let ms_to_samples = |ms: f32| ((SAMPLE_RATE as f32 * ms / 1000.0).round() as usize).max(1);

    let candidates = [
        ("baseline", 0.0f32, 0usize, 0.0f32, 0usize, 0.0f32, 0usize),
        (
            "ch1_t035_w4ms",
            0.35f32,
            ms_to_samples(4.0),
            0.0f32,
            0usize,
            0.0f32,
            0usize,
        ),
        (
            "ch4_t025_w4ms",
            0.0f32,
            0usize,
            0.0f32,
            0usize,
            0.25f32,
            ms_to_samples(4.0),
        ),
        (
            "ch5_t035_w5ms",
            0.0f32,
            0usize,
            0.35f32,
            ms_to_samples(5.0),
            0.0f32,
            0usize,
        ),
        (
            "ch1_t035_w4ms_ch5_t035_w5ms",
            0.35f32,
            ms_to_samples(4.0),
            0.35f32,
            ms_to_samples(5.0),
            0.0f32,
            0usize,
        ),
        (
            "ch4_t025_w4ms_ch5_t035_w5ms",
            0.0f32,
            0usize,
            0.35f32,
            ms_to_samples(5.0),
            0.25f32,
            ms_to_samples(4.0),
        ),
        (
            "ch1_t035_w4ms_ch4_t025_w4ms_ch5_t035_w5ms",
            0.35f32,
            ms_to_samples(4.0),
            0.35f32,
            ms_to_samples(5.0),
            0.25f32,
            ms_to_samples(4.0),
        ),
        (
            "ch1_t050_w6ms_ch4_t035_w5ms_ch5_t050_w6ms",
            0.50f32,
            ms_to_samples(6.0),
            0.50f32,
            ms_to_samples(6.0),
            0.35f32,
            ms_to_samples(5.0),
        ),
    ];
    let mut results = Vec::new();

    for (name, ch1_amount, ch1_window, ch5_amount, ch5_window, ch4_amount, ch4_window) in candidates
    {
        let stem0 =
            apply_side_triggered_transient_mix(&ch1_stem, &ch1_triggers, ch1_window, ch1_amount);
        let stem3 =
            apply_side_triggered_transient_mix(&ch4_stem, &ch4_triggers, ch4_window, ch4_amount);
        let stem4 =
            apply_side_triggered_transient_mix(&ch5_stem, &ch5_triggers, ch5_window, ch5_amount);
        let candidate_linear = sum_stereo_sources(&[
            stem0.as_slice(),
            stem3.as_slice(),
            stem4.as_slice(),
            other_ym_stem.as_slice(),
            psg_stem.as_slice(),
        ]);
        let candidate = scale_and_clamp_stereo(&candidate_linear, final_stem_gain);
        let Some(mono) =
            score_fixed_mono_consensus_candidate(name, &candidate, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(name, &candidate, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(name, &candidate, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name.to_owned(),
            ch1_amount,
            ch1_window,
            ch5_amount,
            ch5_window,
            ch4_amount,
            ch4_window,
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.sort_by(|a, b| {
        b.9.partial_cmp(&a.9)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.7.partial_cmp(&a.7).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ channel stem key-triggered transient sweep ===");
    eprintln!(
        "key_on_counts: ch1={} ch4={} ch5={}",
        ch1_triggers.len(),
        ch4_triggers.len(),
        ch5_triggers.len()
    );
    for (
        name,
        ch1_amount,
        ch1_window,
        ch5_amount,
        ch5_window,
        ch4_amount,
        ch4_window,
        mono_final,
        side_final,
        dyn_final,
        combined,
    ) in results
    {
        eprintln!(
            "{name}: ch1_t={ch1_amount:.2}/{ch1_window} ch5_t={ch5_amount:.2}/{ch5_window} ch4_t={ch4_amount:.2}/{ch4_window} mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_soundlog_tone_change_transient_sweep() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let stem_master_gain = 0.25f32;
    let final_stem_gain = current_default.master_gain / stem_master_gain;
    let linear_stem_config = current_default.with_gain(stem_master_gain);
    let ym_only_config = linear_stem_config.with_psg_gain(0.0);
    let psg_only_config = linear_stem_config.with_ym_gain(0.0);
    let ch1_mask = 1u8 << 0;
    let ch4_mask = 1u8 << 3;
    let ch5_mask = 1u8 << 4;
    let other_mask = 0x3Fu8 & !(ch1_mask | ch4_mask | ch5_mask);
    let render_ym_stem = |keep_mask: u8| {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(ym_only_config);
        let rendered_full = renderer.render_timed_writes(
            &mute_ym_pan_writes_outside_channels(&trace.ym_writes, keep_mask),
            &trace.psg_writes,
            0,
            trace.end_tick,
        );
        rendered_full[capture_start..].to_vec()
    };
    let ch1_stem = render_ym_stem(ch1_mask);
    let ch4_stem = render_ym_stem(ch4_mask);
    let ch5_stem = render_ym_stem(ch5_mask);
    let other_ym_stem = render_ym_stem(other_mask);
    let mut psg_renderer = CoreAudioRenderer::with_audio_output_config(psg_only_config);
    let psg_full =
        psg_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let psg_stem = psg_full[capture_start..].to_vec();

    let events = extract_ym2612_state_events_from_timed_writes(&trace.ym_writes, 0, trace.end_tick)
        .expect("soundlog should extract YM2612 events from GHZ trace");
    let capture_start_sample = trace.capture_start_sample as u64;
    let ch1_tone_triggers = tracked_event_trigger_samples(
        &events,
        capture_start_sample,
        Ym2612TrackedEventKind::ToneChange,
        ch1_mask,
    );
    let ch4_tone_triggers = tracked_event_trigger_samples(
        &events,
        capture_start_sample,
        Ym2612TrackedEventKind::ToneChange,
        ch4_mask,
    );
    let ch5_tone_triggers = tracked_event_trigger_samples(
        &events,
        capture_start_sample,
        Ym2612TrackedEventKind::ToneChange,
        ch5_mask,
    );
    let ms_to_samples = |ms: f32| ((SAMPLE_RATE as f32 * ms / 1000.0).round() as usize).max(1);

    let candidates = [
        ("baseline", 0.0f32, 0usize, 0.0f32, 0usize, 0.0f32, 0usize),
        (
            "ch1_tc_t015_w2ms",
            0.15f32,
            ms_to_samples(2.0),
            0.0f32,
            0usize,
            0.0f32,
            0usize,
        ),
        (
            "ch4_tc_t010_w2ms",
            0.0f32,
            0usize,
            0.0f32,
            0usize,
            0.10f32,
            ms_to_samples(2.0),
        ),
        (
            "ch5_tc_t010_w2ms",
            0.0f32,
            0usize,
            0.10f32,
            ms_to_samples(2.0),
            0.0f32,
            0usize,
        ),
        (
            "ch1_tc_t015_w2ms_ch4_tc_t010_w2ms",
            0.15f32,
            ms_to_samples(2.0),
            0.0f32,
            0usize,
            0.10f32,
            ms_to_samples(2.0),
        ),
        (
            "ch1_tc_t020_w3ms_ch4_tc_t015_w3ms",
            0.20f32,
            ms_to_samples(3.0),
            0.0f32,
            0usize,
            0.15f32,
            ms_to_samples(3.0),
        ),
        (
            "ch1_tc_t020_w3ms_ch4_tc_t015_w3ms_ch5_tc_t010_w2ms",
            0.20f32,
            ms_to_samples(3.0),
            0.10f32,
            ms_to_samples(2.0),
            0.15f32,
            ms_to_samples(3.0),
        ),
    ];
    let mut results = Vec::new();

    for (name, ch1_amount, ch1_window, ch5_amount, ch5_window, ch4_amount, ch4_window) in candidates
    {
        let stem0 = apply_side_triggered_transient_mix(
            &ch1_stem,
            &ch1_tone_triggers,
            ch1_window,
            ch1_amount,
        );
        let stem3 = apply_side_triggered_transient_mix(
            &ch4_stem,
            &ch4_tone_triggers,
            ch4_window,
            ch4_amount,
        );
        let stem4 = apply_side_triggered_transient_mix(
            &ch5_stem,
            &ch5_tone_triggers,
            ch5_window,
            ch5_amount,
        );
        let candidate_linear = sum_stereo_sources(&[
            stem0.as_slice(),
            stem3.as_slice(),
            stem4.as_slice(),
            other_ym_stem.as_slice(),
            psg_stem.as_slice(),
        ]);
        let candidate = scale_and_clamp_stereo(&candidate_linear, final_stem_gain);
        let Some(mono) =
            score_fixed_mono_consensus_candidate(name, &candidate, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(name, &candidate, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(name, &candidate, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name.to_owned(),
            ch1_amount,
            ch1_window,
            ch5_amount,
            ch5_window,
            ch4_amount,
            ch4_window,
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.sort_by(|a, b| {
        b.9.partial_cmp(&a.9)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.7.partial_cmp(&a.7).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ soundlog tone-change transient sweep ===");
    eprintln!(
        "tone_change_counts: ch1={} ch4={} ch5={}",
        ch1_tone_triggers.len(),
        ch4_tone_triggers.len(),
        ch5_tone_triggers.len()
    );
    for (
        name,
        ch1_amount,
        ch1_window,
        ch5_amount,
        ch5_window,
        ch4_amount,
        ch4_window,
        mono_final,
        side_final,
        dyn_final,
        combined,
    ) in results
    {
        eprintln!(
            "{name}: ch1_tc={ch1_amount:.2}/{ch1_window} ch5_tc={ch5_amount:.2}/{ch5_window} ch4_tc={ch4_amount:.2}/{ch4_window} mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_channel_stem_side_delay_sweep() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let stem_master_gain = 0.25f32;
    let final_stem_gain = current_default.master_gain / stem_master_gain;
    let linear_stem_config = current_default.with_gain(stem_master_gain);
    let ym_only_config = linear_stem_config.with_psg_gain(0.0);
    let psg_only_config = linear_stem_config.with_ym_gain(0.0);
    let ch1_mask = 1u8 << 0;
    let ch4_mask = 1u8 << 3;
    let ch5_mask = 1u8 << 4;
    let other_mask = 0x3Fu8 & !(ch1_mask | ch4_mask | ch5_mask);
    let render_ym_stem = |keep_mask: u8| {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(ym_only_config);
        let rendered_full = renderer.render_timed_writes(
            &mute_ym_pan_writes_outside_channels(&trace.ym_writes, keep_mask),
            &trace.psg_writes,
            0,
            trace.end_tick,
        );
        rendered_full[capture_start..].to_vec()
    };
    let ch1_stem = render_ym_stem(ch1_mask);
    let ch4_stem = render_ym_stem(ch4_mask);
    let ch5_stem = render_ym_stem(ch5_mask);
    let other_ym_stem = render_ym_stem(other_mask);
    let mut psg_renderer = CoreAudioRenderer::with_audio_output_config(psg_only_config);
    let psg_full =
        psg_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let psg_stem = psg_full[capture_start..].to_vec();

    let reconstructed_linear = sum_stereo_sources(&[
        ch1_stem.as_slice(),
        ch4_stem.as_slice(),
        ch5_stem.as_slice(),
        other_ym_stem.as_slice(),
        psg_stem.as_slice(),
    ]);
    let reconstructed = scale_and_clamp_stereo(&reconstructed_linear, final_stem_gain);
    let reconstruction_max_diff = anchor_rendered
        .iter()
        .zip(&reconstructed)
        .map(|(&a, &b)| (a - b).abs())
        .fold(0.0f32, f32::max);

    let candidates = [
        ("baseline", 0.0f32, 0usize, 0.0f32, 0usize, 0.0f32, 0usize),
        (
            "ch5_d1_a020",
            0.0f32,
            0usize,
            0.20f32,
            1usize,
            0.0f32,
            0usize,
        ),
        (
            "ch5_d2_a020",
            0.0f32,
            0usize,
            0.20f32,
            2usize,
            0.0f32,
            0usize,
        ),
        (
            "ch5_d1_a035",
            0.0f32,
            0usize,
            0.35f32,
            1usize,
            0.0f32,
            0usize,
        ),
        (
            "ch4_d1_a020",
            0.0f32,
            0usize,
            0.0f32,
            0usize,
            0.20f32,
            1usize,
        ),
        (
            "ch4_d2_a020",
            0.0f32,
            0usize,
            0.0f32,
            0usize,
            0.20f32,
            2usize,
        ),
        (
            "ch1_d1_a020",
            0.20f32,
            1usize,
            0.0f32,
            0usize,
            0.0f32,
            0usize,
        ),
        (
            "ch5_d1_a020_ch4_d1_a020",
            0.0f32,
            0usize,
            0.20f32,
            1usize,
            0.20f32,
            1usize,
        ),
        (
            "ch5_d1_a035_ch4_d1_a020",
            0.0f32,
            0usize,
            0.35f32,
            1usize,
            0.20f32,
            1usize,
        ),
        (
            "ch5_d2_a035_ch4_d1_a020",
            0.0f32,
            0usize,
            0.35f32,
            2usize,
            0.20f32,
            1usize,
        ),
    ];
    let mut results = Vec::new();

    for (name, ch1_amount, ch1_delay, ch5_amount, ch5_delay, ch4_amount, ch4_delay) in candidates {
        let stem0 = apply_side_delay_mix(&ch1_stem, ch1_amount, ch1_delay);
        let stem3 = apply_side_delay_mix(&ch4_stem, ch4_amount, ch4_delay);
        let stem4 = apply_side_delay_mix(&ch5_stem, ch5_amount, ch5_delay);
        let candidate_linear = sum_stereo_sources(&[
            stem0.as_slice(),
            stem3.as_slice(),
            stem4.as_slice(),
            other_ym_stem.as_slice(),
            psg_stem.as_slice(),
        ]);
        let candidate = scale_and_clamp_stereo(&candidate_linear, final_stem_gain);
        let Some(mono) =
            score_fixed_mono_consensus_candidate(name, &candidate, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(name, &candidate, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(name, &candidate, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name.to_owned(),
            ch1_amount,
            ch1_delay,
            ch5_amount,
            ch5_delay,
            ch4_amount,
            ch4_delay,
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.sort_by(|a, b| b.10.partial_cmp(&a.10).unwrap_or(std::cmp::Ordering::Equal));

    eprintln!("=== GHZ channel stem side delay sweep ===");
    eprintln!("reconstruction_max_diff={reconstruction_max_diff:.8}");
    for (
        name,
        ch1_amount,
        ch1_delay,
        ch5_amount,
        ch5_delay,
        ch4_amount,
        ch4_delay,
        mono_final,
        side_final,
        dyn_final,
        combined,
    ) in results
    {
        eprintln!(
            "{name}: ch1_d={ch1_delay} a={ch1_amount:.2} ch5_d={ch5_delay} a={ch5_amount:.2} ch4_d={ch4_delay} a={ch4_amount:.2} mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_channel_stem_side_delay_refine() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let stem_master_gain = 0.25f32;
    let final_stem_gain = current_default.master_gain / stem_master_gain;
    let linear_stem_config = current_default.with_gain(stem_master_gain);
    let ym_only_config = linear_stem_config.with_psg_gain(0.0);
    let psg_only_config = linear_stem_config.with_ym_gain(0.0);
    let ch1_mask = 1u8 << 0;
    let ch4_mask = 1u8 << 3;
    let ch5_mask = 1u8 << 4;
    let other_mask = 0x3Fu8 & !(ch1_mask | ch4_mask | ch5_mask);
    let render_ym_stem = |keep_mask: u8| {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(ym_only_config);
        let rendered_full = renderer.render_timed_writes(
            &mute_ym_pan_writes_outside_channels(&trace.ym_writes, keep_mask),
            &trace.psg_writes,
            0,
            trace.end_tick,
        );
        rendered_full[capture_start..].to_vec()
    };
    let ch1_stem = render_ym_stem(ch1_mask);
    let ch4_stem = render_ym_stem(ch4_mask);
    let ch5_stem = render_ym_stem(ch5_mask);
    let other_ym_stem = render_ym_stem(other_mask);
    let mut psg_renderer = CoreAudioRenderer::with_audio_output_config(psg_only_config);
    let psg_full =
        psg_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let psg_stem = psg_full[capture_start..].to_vec();

    let candidates = [
        (
            "baseline", 0.00f32, 0usize, 0.00f32, 0usize, 0.00f32, 0usize,
        ),
        (
            "ch1_d1_a010",
            0.10f32,
            1usize,
            0.00f32,
            0usize,
            0.00f32,
            0usize,
        ),
        (
            "ch1_d1_a020",
            0.20f32,
            1usize,
            0.00f32,
            0usize,
            0.00f32,
            0usize,
        ),
        (
            "ch1_d1_a030",
            0.30f32,
            1usize,
            0.00f32,
            0usize,
            0.00f32,
            0usize,
        ),
        (
            "ch1_d1_a040",
            0.40f32,
            1usize,
            0.00f32,
            0usize,
            0.00f32,
            0usize,
        ),
        (
            "ch1_d2_a010",
            0.10f32,
            2usize,
            0.00f32,
            0usize,
            0.00f32,
            0usize,
        ),
        (
            "ch1_d2_a020",
            0.20f32,
            2usize,
            0.00f32,
            0usize,
            0.00f32,
            0usize,
        ),
        (
            "ch1_d2_a030",
            0.30f32,
            2usize,
            0.00f32,
            0usize,
            0.00f32,
            0usize,
        ),
        (
            "ch1_d1_a020_ch4_d1_a010",
            0.20f32,
            1usize,
            0.00f32,
            0usize,
            0.10f32,
            1usize,
        ),
        (
            "ch1_d1_a020_ch4_d1_a020",
            0.20f32,
            1usize,
            0.00f32,
            0usize,
            0.20f32,
            1usize,
        ),
        (
            "ch1_d1_a020_ch5_d1_a010",
            0.20f32,
            1usize,
            0.10f32,
            1usize,
            0.00f32,
            0usize,
        ),
        (
            "ch1_d1_a020_ch5_d1_a020",
            0.20f32,
            1usize,
            0.20f32,
            1usize,
            0.00f32,
            0usize,
        ),
    ];
    let mut results = Vec::new();

    for (name, ch1_amount, ch1_delay, ch5_amount, ch5_delay, ch4_amount, ch4_delay) in candidates {
        let stem0 = apply_side_delay_mix(&ch1_stem, ch1_amount, ch1_delay);
        let stem3 = apply_side_delay_mix(&ch4_stem, ch4_amount, ch4_delay);
        let stem4 = apply_side_delay_mix(&ch5_stem, ch5_amount, ch5_delay);
        let candidate_linear = sum_stereo_sources(&[
            stem0.as_slice(),
            stem3.as_slice(),
            stem4.as_slice(),
            other_ym_stem.as_slice(),
            psg_stem.as_slice(),
        ]);
        let candidate = scale_and_clamp_stereo(&candidate_linear, final_stem_gain);
        let Some(mono) =
            score_fixed_mono_consensus_candidate(name, &candidate, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(name, &candidate, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(name, &candidate, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name.to_owned(),
            ch1_amount,
            ch1_delay,
            ch5_amount,
            ch5_delay,
            ch4_amount,
            ch4_delay,
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.sort_by(|a, b| b.10.partial_cmp(&a.10).unwrap_or(std::cmp::Ordering::Equal));

    eprintln!("=== GHZ channel stem side delay refine ===");
    for (
        name,
        ch1_amount,
        ch1_delay,
        ch5_amount,
        ch5_delay,
        ch4_amount,
        ch4_delay,
        mono_final,
        side_final,
        dyn_final,
        combined,
    ) in results
    {
        eprintln!(
            "{name}: ch1_d={ch1_delay} a={ch1_amount:.2} ch5_d={ch5_delay} a={ch5_amount:.2} ch4_d={ch4_delay} a={ch4_amount:.2} mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_channel_stem_delay_transient_hybrid() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let stem_master_gain = 0.25f32;
    let final_stem_gain = current_default.master_gain / stem_master_gain;
    let linear_stem_config = current_default.with_gain(stem_master_gain);
    let ym_only_config = linear_stem_config.with_psg_gain(0.0);
    let psg_only_config = linear_stem_config.with_ym_gain(0.0);
    let ch1_mask = 1u8 << 0;
    let ch4_mask = 1u8 << 3;
    let ch5_mask = 1u8 << 4;
    let other_mask = 0x3Fu8 & !(ch1_mask | ch4_mask | ch5_mask);
    let render_ym_stem = |keep_mask: u8| {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(ym_only_config);
        let rendered_full = renderer.render_timed_writes(
            &mute_ym_pan_writes_outside_channels(&trace.ym_writes, keep_mask),
            &trace.psg_writes,
            0,
            trace.end_tick,
        );
        rendered_full[capture_start..].to_vec()
    };
    let ch1_stem = render_ym_stem(ch1_mask);
    let ch4_stem = render_ym_stem(ch4_mask);
    let ch5_stem = render_ym_stem(ch5_mask);
    let other_ym_stem = render_ym_stem(other_mask);
    let mut psg_renderer = CoreAudioRenderer::with_audio_output_config(psg_only_config);
    let psg_full =
        psg_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let psg_stem = psg_full[capture_start..].to_vec();

    let candidates = [
        ("baseline", 0.0f32, 0.0f32, 0.0f32, 0.0f32),
        ("delay_base", 0.20f32, 0.20f32, 0.0f32, 0.0f32),
        ("delay_base_ch5_t020", 0.20f32, 0.20f32, 0.20f32, 0.0f32),
        ("delay_base_ch5_t035", 0.20f32, 0.20f32, 0.35f32, 0.0f32),
        (
            "delay_base_ch5_t020_ch4_t010",
            0.20f32,
            0.20f32,
            0.20f32,
            0.10f32,
        ),
        (
            "delay_base_ch5_t020_ch4_t020",
            0.20f32,
            0.20f32,
            0.20f32,
            0.20f32,
        ),
        (
            "delay_base_ch5_t035_ch4_t010",
            0.20f32,
            0.20f32,
            0.35f32,
            0.10f32,
        ),
        (
            "delay_base_ch5_t035_ch4_t020",
            0.20f32,
            0.20f32,
            0.35f32,
            0.20f32,
        ),
    ];
    let mut results = Vec::new();

    for (name, ch1_delay_amount, ch4_delay_amount, ch5_transient, ch4_transient) in candidates {
        let stem0 = apply_side_delay_mix(&ch1_stem, ch1_delay_amount, 1);
        let delayed_ch4 = apply_side_delay_mix(&ch4_stem, ch4_delay_amount, 1);
        let stem3 = apply_side_transient_mix(&delayed_ch4, ch4_transient);
        let stem4 = apply_side_transient_mix(&ch5_stem, ch5_transient);
        let candidate_linear = sum_stereo_sources(&[
            stem0.as_slice(),
            stem3.as_slice(),
            stem4.as_slice(),
            other_ym_stem.as_slice(),
            psg_stem.as_slice(),
        ]);
        let candidate = scale_and_clamp_stereo(&candidate_linear, final_stem_gain);
        let Some(mono) =
            score_fixed_mono_consensus_candidate(name, &candidate, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(name, &candidate, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(name, &candidate, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name.to_owned(),
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.sort_by(|a, b| b.4.partial_cmp(&a.4).unwrap_or(std::cmp::Ordering::Equal));

    eprintln!("=== GHZ channel stem delay + transient hybrid ===");
    for (name, mono_final, side_final, dyn_final, combined) in results {
        eprintln!(
            "{name}: mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_side_transient_mix_candidates() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let rendered_full =
        renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(rendered_full.len());
    let base_rendered = &rendered_full[capture_start..];

    let Some(section_targets) =
        build_fixed_sectioned_side_dynamics(base_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let candidates = [
        ("current_default", base_rendered.to_vec()),
        (
            "transient_0_10",
            apply_side_transient_mix(base_rendered, 0.10),
        ),
        (
            "transient_0_20",
            apply_side_transient_mix(base_rendered, 0.20),
        ),
        (
            "transient_0_35",
            apply_side_transient_mix(base_rendered, 0.35),
        ),
        (
            "transient_0_50",
            apply_side_transient_mix(base_rendered, 0.50),
        ),
    ];

    let current_sections = analyze_fixed_sectioned_side_dynamics_candidate_sections(
        "current_default",
        base_rendered,
        &section_targets,
    )
    .expect("expected current_default side-dynamics analysis");
    let mut weakest: Vec<_> = current_sections
        .iter()
        .enumerate()
        .map(|(idx, section)| (idx, section.final_score))
        .collect();
    weakest.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    eprintln!("=== GHZ side transient mix candidates ===");
    for (name, rendered) in &candidates {
        let Some(score) =
            score_fixed_sectioned_side_dynamics_candidate(name, rendered, &section_targets)
        else {
            continue;
        };
        let Some(analyzed) = analyze_fixed_sectioned_side_dynamics_candidate_sections(
            name,
            rendered,
            &section_targets,
        ) else {
            continue;
        };
        eprintln!(
            "{name:>18}: average={:.4} penalty={:.4} final={:.4} worst={:.4}",
            score.average_score,
            score.disagreement_penalty,
            score.final_score,
            score.worst_section_score
        );
        for (idx, _) in weakest.iter().take(3) {
            let target = &section_targets[*idx];
            let section = &analyzed[*idx];
            let target_transient = envelope_transient_profile(&target.target.envelope);
            let candidate_transient = envelope_transient_profile(&section.candidate.envelope);
            let (lag_bins, adjusted_env) = best_envelope_offset_bins(
                &section.candidate.envelope,
                &target.target.envelope,
                SIDE_DYNAMICS_MAX_LAG_BINS,
            )
            .unwrap_or((
                0,
                cross_correlation(&section.candidate.envelope, &target.target.envelope),
            ));
            let transient = cross_correlation(&candidate_transient, &target_transient);
            let adjusted_transient =
                correlation_at_offset_bins(&candidate_transient, &target_transient, lag_bins);
            let transient_rms = rms(&candidate_transient) / rms(&target_transient).max(1e-9);
            eprintln!(
                "  {:.2}s..{:.2}s: final={:.4} env={:.4}->{adjusted_env:.4} trans={:.4}->{adjusted_transient:.4} trans_rms={transient_rms:.4} ratio={:.4}",
                target.start as f32 / SAMPLE_RATE as f32,
                (target.start + target.len) as f32 / SAMPLE_RATE as f32,
                section.final_score,
                cross_correlation(&section.candidate.envelope, &target.target.envelope),
                transient,
                section.candidate.ratio,
            );
        }
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_weak_section_reference_side_transients() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let rendered_full =
        renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(rendered_full.len());
    let base_rendered = &rendered_full[capture_start..];

    let Some(section_targets) =
        build_fixed_sectioned_side_dynamics(base_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };
    let current_sections = analyze_fixed_sectioned_side_dynamics_candidate_sections(
        "current_default",
        base_rendered,
        &section_targets,
    )
    .expect("expected current_default side-dynamics analysis");
    let mut weakest: Vec<_> = current_sections
        .iter()
        .enumerate()
        .map(|(idx, section)| (idx, section.final_score))
        .collect();
    weakest.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    eprintln!("=== GHZ weak-section reference side transients ===");
    for (idx, _) in weakest.into_iter().take(4) {
        let target = &section_targets[idx];
        let current = &current_sections[idx];
        let Some(pairs) = analyze_reference_side_dynamics_pairs(&loaded_refs, target) else {
            eprintln!(
                "section {:.2}s..{:.2}s: unable to analyze reference transient pairs",
                target.start as f32 / SAMPLE_RATE as f32,
                (target.start + target.len) as f32 / SAMPLE_RATE as f32,
            );
            continue;
        };
        let Some(summary) = summarize_reference_side_dynamics_pairs(&pairs) else {
            continue;
        };
        let lag_ms = summary.average_abs_lag_bins * SIDE_DYNAMICS_ENV_WINDOW as f32 * 1000.0
            / SAMPLE_RATE as f32;
        eprintln!(
            "section {:.2}s..{:.2}s: current_final={:.4} current_env={:.4} target_ratio={:.4} ref_pairs={} env={:.4}->{:.4} trans={:.4}->{:.4} trans_rms_fit={:.4} avg_abs_lag={:.1}ms worst_adj_trans={:.4}",
            target.start as f32 / SAMPLE_RATE as f32,
            (target.start + target.len) as f32 / SAMPLE_RATE as f32,
            current.final_score,
            cross_correlation(&current.candidate.envelope, &target.target.envelope),
            target.target.ratio,
            summary.pair_count,
            summary.average_envelope_corr,
            summary.average_adjusted_envelope_corr,
            summary.average_transient_corr,
            summary.average_adjusted_transient_corr,
            summary.average_transient_rms_fit,
            lag_ms,
            summary.worst_adjusted_transient_corr,
        );

        for pair in &pairs {
            let pair_lag_ms =
                pair.metrics.lag_bins as f32 * SIDE_DYNAMICS_ENV_WINDOW as f32 * 1000.0
                    / SAMPLE_RATE as f32;
            eprintln!(
                "  {} <-> {}: env={:.4}->{:.4} trans={:.4}->{:.4} trans_rms={:.4} ratio={:.4}/{:.4} delta={:+.4} lag={:+} ({:+.1}ms)",
                pair.left_name,
                pair.right_name,
                pair.metrics.envelope_corr,
                pair.metrics.adjusted_envelope_corr,
                pair.metrics.transient_corr,
                pair.metrics.adjusted_transient_corr,
                pair.metrics.transient_rms_ratio,
                pair.left_ratio,
                pair.right_ratio,
                pair.left_ratio - pair.right_ratio,
                pair.metrics.lag_bins,
                pair_lag_ms,
            );
        }
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_trusted_window_side_profiles() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let ref_dir = Path::new(REFERENCE_DIR);
    let ref_path = ["sonic_ghz.flac", "sonic_ghz.wav"]
        .iter()
        .map(|f| ref_dir.join(f))
        .find(|p| p.exists())
        .expect("expected GHZ reference audio");
    let (_ref_rate, ref_samples) = load_reference(&ref_path).expect("failed to load reference");

    let record_frames = ((ref_samples.len() as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let ref_left: Vec<f32> = ref_samples.iter().step_by(2).copied().collect();
    let env_window = 2048usize;
    let ref_env = rms_envelope(&ref_left, env_window);
    let spectral_bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 12_000.0, 24);

    let mut base_renderer =
        CoreAudioRenderer::with_audio_output_config(AudioOutputConfig::default());
    let base_rendered_full =
        base_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(base_rendered_full.len());
    let base_rendered = &base_rendered_full[capture_start..];
    let base_left: Vec<f32> = base_rendered.iter().step_by(2).copied().collect();
    let base_env = rms_envelope(&base_left, env_window);
    let max_lag_windows = base_env.len().min(ref_env.len()).saturating_sub(64);
    let (_env_corr, env_lag) = best_lagged_correlation(&base_env, &ref_env, max_lag_windows);
    let local_env_window = (SAMPLE_RATE as usize * 8 / env_window).max(64);
    let local_match =
        best_windowed_match_at_lag(&base_env, &ref_env, env_lag, local_env_window).unwrap();
    let loop_offset_bins = local_match.b_start.abs_diff(local_match.a_start);
    let trusted_env = best_windowed_match_at_lag(
        &ref_env,
        &ref_env,
        loop_offset_bins as isize,
        local_env_window,
    )
    .expect("expected trusted reference loop window");
    let trusted_prior_start = trusted_env.b_start * env_window;
    let trusted_ref_start = trusted_env.a_start * env_window;
    let trusted_len = (trusted_env.len * env_window)
        .min(ref_left.len().saturating_sub(trusted_prior_start))
        .min(ref_left.len().saturating_sub(trusted_ref_start));
    let lag_samples = env_lag * env_window as isize;
    let trusted_emu_start = (trusted_ref_start as isize + lag_samples) as usize;
    let ref_stereo = stereo_window(&ref_samples, trusted_ref_start, trusted_len);
    let (ref_mid, ref_side) = stereo_mid_side(ref_stereo);
    let ref_left_window = &ref_left[trusted_ref_start..trusted_ref_start + trusted_len];

    let current_default = AudioOutputConfig::default();
    let candidates = [
        ("current_default", current_default),
        (
            "no_side_eq",
            current_default
                .with_side_gain(1.0)
                .with_post_side_eq_1(AudioEqStage::peaking(420.0, 0.95, 0.0))
                .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 0.0)),
        ),
        (
            "side_air_plus",
            current_default
                .with_post_side_eq_1(AudioEqStage::peaking(420.0, 0.95, -1.6))
                .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 1.6)),
        ),
        (
            "side_lowmid_cut_more",
            current_default
                .with_post_side_eq_1(AudioEqStage::peaking(420.0, 0.95, -2.6))
                .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 0.8)),
        ),
        (
            "side_gain_down",
            current_default
                .with_side_gain(0.9)
                .with_post_side_eq_1(AudioEqStage::peaking(420.0, 0.95, -1.6))
                .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 0.8)),
        ),
        (
            "side_gain_up",
            current_default
                .with_side_gain(1.1)
                .with_post_side_eq_1(AudioEqStage::peaking(420.0, 0.95, -1.6))
                .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 0.8)),
        ),
    ];

    eprintln!(
        "=== GHZ trusted-window side sweep === trusted_prior={:.2}s trusted_current={:.2}s len={:.2}s",
        trusted_prior_start as f64 / SAMPLE_RATE as f64,
        trusted_ref_start as f64 / SAMPLE_RATE as f64,
        trusted_len as f64 / SAMPLE_RATE as f64
    );

    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        if trusted_emu_start + trusted_len > rendered.len() / 2 {
            continue;
        }
        let emu_stereo = stereo_window(rendered, trusted_emu_start, trusted_len);
        let (emu_mid, emu_side) = stereo_mid_side(emu_stereo);
        let emu_left: Vec<f32> = emu_stereo.iter().step_by(2).copied().collect();
        let side_spectral = spectral_similarity(&emu_side, &ref_side, 2048, 1024, &spectral_bins);
        let mid_spectral = spectral_similarity(&emu_mid, &ref_mid, 2048, 1024, &spectral_bins);
        let left_spectral =
            spectral_similarity(&emu_left, ref_left_window, 2048, 1024, &spectral_bins);
        let rms_ratio = rms(&emu_left) / rms(ref_left_window).max(1e-9);
        eprintln!(
            "{name:>20}: left={left_spectral:.4} mid={mid_spectral:.4} side={side_spectral:.4} rms={rms_ratio:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_reference_consensus() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            eprintln!("Skipping {} due to sample rate {}", path.display(), rate);
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let side_candidate = current_default
        .with_post_side_eq_1(AudioEqStage::peaking(420.0, 0.95, -2.6))
        .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 0.8));
    let candidates = [
        ("current_default", current_default),
        ("side_lowmid_cut_more", side_candidate),
    ];

    eprintln!("=== GHZ reference consensus ===");
    for (ref_path, ref_samples) in loaded_refs {
        let ref_name = ref_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        eprintln!("-- {ref_name} --");
        for (name, config) in candidates {
            let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
            let rendered_full = renderer.render_timed_writes(
                &trace.ym_writes,
                &trace.psg_writes,
                0,
                trace.end_tick,
            );
            let capture_start = (trace.capture_start_sample as usize)
                .saturating_mul(2)
                .min(rendered_full.len());
            let rendered = &rendered_full[capture_start..];
            let Some(metrics) = analyze_trusted_reference_window(rendered, &ref_samples) else {
                eprintln!("{name:>20}: unable to analyze trusted window");
                continue;
            };
            eprintln!(
                "{name:>20}: env={:.4} local_env={:.4} trusted_score={:.4} self=({:.4}/{:.4}/{:.4}/{:.4}) emu=({:.4}/{:.4}/{:.4}/{:.4}/{:.4})",
                metrics.env_corr,
                metrics.local_env_corr,
                metrics.trusted_score,
                metrics.trusted_self_left,
                metrics.trusted_self_mid,
                metrics.trusted_self_side,
                metrics.trusted_self_rms,
                metrics.trusted_emu_raw,
                metrics.trusted_emu_spectral,
                metrics.trusted_emu_rms,
                metrics.trusted_emu_mid,
                metrics.trusted_emu_side,
            );
        }
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_consensus_profiles() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            eprintln!("Skipping {} due to sample rate {}", path.display(), rate);
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);

    let mut scored = Vec::new();
    for (name, config) in consensus_profile_candidates() {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];

        let mut metrics = Vec::new();
        for (_, ref_samples) in &loaded_refs {
            if let Some(metric) = analyze_trusted_reference_window(rendered, ref_samples) {
                metrics.push(metric);
            }
        }
        if metrics.is_empty() {
            continue;
        }
        scored.push(score_consensus_candidate(name, &metrics));
    }

    scored.sort_by(|a, b| {
        b.final_score
            .partial_cmp(&a.final_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.average_score
                    .partial_cmp(&a.average_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    eprintln!("=== GHZ weighted consensus profile sweep ===");
    for score in scored {
        eprintln!(
            "{:>20}: average={:.4} penalty={:.4} final={:.4}",
            score.name, score.average_score, score.disagreement_penalty, score.final_score
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_consensus_leaders() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.is_empty() {
        eprintln!("No 44.1kHz GHZ references found, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let candidates = [
        ("current_default", AudioOutputConfig::default()),
        (
            "xf_0_30",
            AudioOutputConfig::default().with_stereo_crossfeed(0.30),
        ),
        ("psg_0_90", AudioOutputConfig::default().with_psg_gain(0.90)),
    ];

    eprintln!("=== GHZ consensus leaders ===");
    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        let mut metrics = Vec::new();
        eprintln!("-- {name} --");
        for (ref_path, ref_samples) in &loaded_refs {
            let ref_name = ref_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");
            let Some(metric) = analyze_trusted_reference_window(rendered, ref_samples) else {
                continue;
            };
            eprintln!(
                "{ref_name:>20}: weight={:.4} norm={:.4} local_env={:.4} trusted=({:.4}/{:.4}/{:.4}/{:.4}/{:.4})",
                reference_reliability_weight(metric),
                normalized_reference_score(metric),
                metric.local_env_corr,
                metric.trusted_emu_spectral,
                metric.trusted_emu_rms,
                metric.trusted_emu_mid,
                metric.trusted_emu_side,
                trusted_reference_score(metric),
            );
            metrics.push(metric);
        }
        let score = score_consensus_candidate(name, &metrics);
        eprintln!(
            "{name:>20}: average={:.4} penalty={:.4} final={:.4}",
            score.average_score, score.disagreement_penalty, score.final_score
        );
    }
}

/// Generate emulator audio: skip `skip_frames`, then record `record_frames`.
fn generate_emu_audio_with_config(
    rom: &[u8],
    skip_frames: u32,
    record_frames: u32,
    config: AudioOutputConfig,
) -> Vec<f32> {
    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom.to_vec()));
    core.execute(Command::SetAudioSampleRate(SAMPLE_RATE));
    core.execute(Command::SetAudioOutputConfig(config));

    for _ in 0..skip_frames {
        core.execute(Command::StepFrame);
    }
    core.clear_audio_buffer();

    let mut all_samples = Vec::new();
    for _ in 0..record_frames {
        core.execute(Command::StepFrame);
        all_samples.extend_from_slice(core.audio_samples());
        core.clear_audio_buffer();
    }
    all_samples
}

fn generate_emu_audio(rom: &[u8], skip_frames: u32, record_frames: u32) -> Vec<f32> {
    generate_emu_audio_with_config(
        rom,
        skip_frames,
        record_frames,
        AudioOutputConfig::default(),
    )
}

/// Generate emulator audio after booting into Green Hill Zone gameplay.
fn generate_ghz_audio_with_config(
    rom: &[u8],
    record_frames: u32,
    config: AudioOutputConfig,
) -> Vec<f32> {
    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom.to_vec()));
    core.execute(Command::SetAudioSampleRate(SAMPLE_RATE));
    core.execute(Command::SetAudioOutputConfig(config));
    let _extra_frames = advance_to_green_hill_music(&mut core);

    let mut all_samples = Vec::new();
    for _ in 0..record_frames {
        core.execute(Command::StepFrame);
        all_samples.extend_from_slice(core.audio_samples());
        core.clear_audio_buffer();
    }
    all_samples
}

fn generate_ghz_audio(rom: &[u8], record_frames: u32) -> Vec<f32> {
    generate_ghz_audio_with_config(rom, record_frames, AudioOutputConfig::default())
}

fn with_capture_eq(
    base: AudioOutputConfig,
    low_shelf_hz: f32,
    low_shelf_db: f32,
    peak_hz: f32,
    peak_q: f32,
    peak_db: f32,
    high_shelf_hz: f32,
    high_shelf_db: f32,
) -> AudioOutputConfig {
    base.with_post_eq_1(AudioEqStage::low_shelf(low_shelf_hz, low_shelf_db))
        .with_post_eq_2(AudioEqStage::peaking(peak_hz, peak_q, peak_db))
        .with_post_eq_3(AudioEqStage::high_shelf(high_shelf_hz, high_shelf_db))
}

#[allow(clippy::too_many_arguments)]
fn with_capture_eq_extended(
    base: AudioOutputConfig,
    low_shelf_hz: f32,
    low_shelf_db: f32,
    peak_hz: f32,
    peak_q: f32,
    peak_db: f32,
    high_shelf_hz: f32,
    high_shelf_db: f32,
    extra_peak_1_hz: f32,
    extra_peak_1_q: f32,
    extra_peak_1_db: f32,
    extra_peak_2_hz: f32,
    extra_peak_2_q: f32,
    extra_peak_2_db: f32,
) -> AudioOutputConfig {
    with_capture_eq(
        base,
        low_shelf_hz,
        low_shelf_db,
        peak_hz,
        peak_q,
        peak_db,
        high_shelf_hz,
        high_shelf_db,
    )
    .with_post_eq_4(AudioEqStage::peaking(
        extra_peak_1_hz,
        extra_peak_1_q,
        extra_peak_1_db,
    ))
    .with_post_eq_5(AudioEqStage::peaking(
        extra_peak_2_hz,
        extra_peak_2_q,
        extra_peak_2_db,
    ))
}

fn capture_ghz_timed_trace(rom: &[u8], record_frames: u32) -> GhzTimedTrace {
    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom.to_vec()));
    core.execute(Command::SetAudioSampleRate(SAMPLE_RATE));
    core.execute(Command::SetAudioOutputConfig(AudioOutputConfig::default()));
    core.clear_audio_buffer();
    core.clear_ym2612_timed_write_trace();
    core.clear_psg_timed_write_trace();

    let extra_frames = advance_to_green_hill_music(&mut core);
    let capture_start_tick = core.audio_master_ticks();
    let capture_start_sample = core.audio_output_sample_count();
    core.clear_audio_buffer();

    for _ in 0..record_frames {
        core.execute(Command::StepFrame);
        core.clear_audio_buffer();
    }

    GhzTimedTrace {
        extra_frames,
        capture_start_tick,
        capture_start_sample,
        end_tick: core.audio_master_ticks(),
        ym_writes: core.ym2612_timed_write_trace().to_vec(),
        psg_writes: core.psg_timed_write_trace().to_vec(),
    }
}

fn ym2612_event_kind_index(kind: Ym2612TrackedEventKind) -> usize {
    match kind {
        Ym2612TrackedEventKind::KeyOn => 0,
        Ym2612TrackedEventKind::KeyOff => 1,
        Ym2612TrackedEventKind::ToneChange => 2,
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_soundlog_ym2612_events() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let trace = capture_ghz_timed_trace(&rom, 600);
    let events = extract_ym2612_state_events_from_timed_writes(&trace.ym_writes, 0, trace.end_tick)
        .expect("soundlog should extract YM2612 events from GHZ trace");
    let capture_start_sample = trace.capture_start_sample as i64;

    let mut overall = [[0u32; 3]; 6];
    let mut overall_total = 0u32;
    for event in &events {
        let rel_sample = i64::from(event.sample) - capture_start_sample;
        if rel_sample < 0 {
            continue;
        }
        overall_total += 1;
        overall[event.channel as usize][ym2612_event_kind_index(event.kind)] += 1;
    }

    eprintln!(
        "\n=== GHZ soundlog YM2612 events === extra_frames={} capture_start_sample={} total_events_after_capture={} total_trace_writes={}",
        trace.extra_frames,
        trace.capture_start_sample,
        overall_total,
        trace.ym_writes.len()
    );
    for (channel, counts) in overall.iter().enumerate() {
        eprintln!(
            "  ch{} total: key_on={} key_off={} tone_change={}",
            channel + 1,
            counts[0],
            counts[1],
            counts[2]
        );
    }

    for second in 0..10 {
        let window_start = capture_start_sample + second as i64 * SAMPLE_RATE as i64;
        let window_end = window_start + SAMPLE_RATE as i64;
        let mut counts = [[0u32; 3]; 6];
        let mut window_events = Vec::new();
        for event in &events {
            let sample = i64::from(event.sample);
            if sample < window_start || sample >= window_end {
                continue;
            }
            counts[event.channel as usize][ym2612_event_kind_index(event.kind)] += 1;
            window_events.push(event);
        }

        if window_events.is_empty() {
            continue;
        }

        eprintln!(
            "\n  capture+{:>2}.0s..{:>2}.0s events={}",
            second,
            second + 1,
            window_events.len()
        );
        for (channel, channel_counts) in counts.iter().enumerate() {
            if channel_counts.iter().all(|&count| count == 0) {
                continue;
            }
            eprintln!(
                "    ch{} key_on={} key_off={} tone_change={}",
                channel + 1,
                channel_counts[0],
                channel_counts[1],
                channel_counts[2]
            );
        }
        for event in window_events.iter().take(8) {
            let rel_ms = (f64::from(event.sample) - trace.capture_start_sample as f64) * 1000.0
                / f64::from(SAMPLE_RATE);
            let kind = match event.kind {
                Ym2612TrackedEventKind::KeyOn => "KeyOn",
                Ym2612TrackedEventKind::KeyOff => "KeyOff",
                Ym2612TrackedEventKind::ToneChange => "ToneChange",
            };
            match event.freq_hz {
                Some(freq_hz) => eprintln!(
                    "      t={rel_ms:8.2} ms ch{} {:>10} freq={freq_hz:8.2} Hz",
                    event.channel + 1,
                    kind
                ),
                None => eprintln!(
                    "      t={rel_ms:8.2} ms ch{} {:>10}",
                    event.channel + 1,
                    kind
                ),
            }
        }
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_soundlog_dominant_sections() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some(consensus_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let current_consensus = analyze_fixed_sectioned_side_consensus_candidate_sections(
        "current_default",
        anchor_rendered,
        &consensus_targets,
    )
    .expect("expected current_default consensus analysis");
    let current_dynamics = analyze_fixed_sectioned_side_dynamics_candidate_sections(
        "current_default",
        anchor_rendered,
        &dynamics_targets,
    )
    .expect("expected current_default dynamics analysis");

    let consensus_finals: Vec<f32> = current_consensus
        .iter()
        .map(|section| section.final_score)
        .collect();
    let consensus_weights: Vec<f32> = consensus_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let dynamics_finals: Vec<f32> = current_dynamics
        .iter()
        .map(|section| section.final_score)
        .collect();
    let dynamics_weights: Vec<f32> = dynamics_targets
        .iter()
        .map(|section| section.weight)
        .collect();

    let (consensus_idx, consensus_impact) =
        dominant_section_impact_index(&consensus_finals, &consensus_weights)
            .expect("expected dominant consensus section");
    let (dynamics_idx, dynamics_impact) =
        dominant_section_impact_index(&dynamics_finals, &dynamics_weights)
            .expect("expected dominant dynamics section");

    let events = extract_ym2612_state_events_from_timed_writes(&trace.ym_writes, 0, trace.end_tick)
        .expect("soundlog should extract YM2612 events from GHZ trace");

    let dominant_sections = [
        (
            "side",
            &consensus_targets[consensus_idx].refs,
            consensus_targets[consensus_idx].start,
            consensus_targets[consensus_idx].len,
            current_consensus[consensus_idx].final_score,
            consensus_impact,
        ),
        (
            "dynamics",
            &dynamics_targets[dynamics_idx].refs,
            dynamics_targets[dynamics_idx].start,
            dynamics_targets[dynamics_idx].len,
            current_dynamics[dynamics_idx].final_score,
            dynamics_impact,
        ),
    ];

    eprintln!("=== GHZ soundlog dominant sections ===");
    for (label, refs, start, len, final_score, impact) in dominant_sections {
        let (positions, start_sample, end_sample) =
            section_sample_range_from_refs(&trace, refs, len)
                .expect("expected dominant section sample range");
        let (counts, total_events) =
            summarize_ym2612_tracked_events_in_sample_range(&events, start_sample, end_sample);
        let window_events: Vec<_> = events
            .iter()
            .filter(|event| {
                let sample = u64::from(event.sample);
                sample >= start_sample && sample < end_sample
            })
            .collect();

        eprintln!(
            "-- dominant {label} section local={:.2}s..{:.2}s abs=[{:.2}s,{:.2}s,{:.2}s..{:.2}s] final={final_score:.4} impact={impact:.4} total_events={total_events} --",
            start as f32 / SAMPLE_RATE as f32,
            (start + len) as f32 / SAMPLE_RATE as f32,
            positions.min_start as f32 / SAMPLE_RATE as f32,
            positions.mean_start as f32 / SAMPLE_RATE as f32,
            positions.max_start as f32 / SAMPLE_RATE as f32,
            positions.max_end as f32 / SAMPLE_RATE as f32,
        );

        for (channel, channel_counts) in counts.iter().enumerate() {
            let channel_total: u32 = channel_counts.iter().sum();
            if channel_total == 0 {
                continue;
            }

            eprintln!(
                "  ch{}: key_on={} key_off={} tone_change={} total={channel_total}",
                channel + 1,
                channel_counts[ym2612_event_kind_index(Ym2612TrackedEventKind::KeyOn)],
                channel_counts[ym2612_event_kind_index(Ym2612TrackedEventKind::KeyOff)],
                channel_counts[ym2612_event_kind_index(Ym2612TrackedEventKind::ToneChange)],
            );
        }

        for event in window_events.iter().take(12) {
            let rel_ms = (f64::from(event.sample) - trace.capture_start_sample as f64) * 1000.0
                / f64::from(SAMPLE_RATE);
            let kind = match event.kind {
                Ym2612TrackedEventKind::KeyOn => "KeyOn",
                Ym2612TrackedEventKind::KeyOff => "KeyOff",
                Ym2612TrackedEventKind::ToneChange => "ToneChange",
            };
            match event.freq_hz {
                Some(freq_hz) => eprintln!(
                    "      t={rel_ms:8.2} ms ch{} {:>10} freq={freq_hz:8.2} Hz",
                    event.channel + 1,
                    kind
                ),
                None => eprintln!(
                    "      t={rel_ms:8.2} ms ch{} {:>10}",
                    event.channel + 1,
                    kind
                ),
            }
        }
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_dominant_tone_change_causality() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let current_consensus = analyze_fixed_sectioned_side_consensus_candidate_sections(
        "current_default",
        anchor_rendered,
        &side_targets,
    )
    .expect("expected current_default consensus analysis");
    let current_dynamics = analyze_fixed_sectioned_side_dynamics_candidate_sections(
        "current_default",
        anchor_rendered,
        &dynamics_targets,
    )
    .expect("expected current_default dynamics analysis");

    let consensus_finals: Vec<f32> = current_consensus
        .iter()
        .map(|section| section.final_score)
        .collect();
    let consensus_weights: Vec<f32> = side_targets.iter().map(|section| section.weight).collect();
    let dynamics_finals: Vec<f32> = current_dynamics
        .iter()
        .map(|section| section.final_score)
        .collect();
    let dynamics_weights: Vec<f32> = dynamics_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let (consensus_idx, _) = dominant_section_impact_index(&consensus_finals, &consensus_weights)
        .expect("expected dominant consensus section");
    let (dynamics_idx, _) = dominant_section_impact_index(&dynamics_finals, &dynamics_weights)
        .expect("expected dominant dynamics section");

    let (_, side_start_tick, side_end_tick) = section_tick_range_from_refs(
        &trace,
        &side_targets[consensus_idx].refs,
        side_targets[consensus_idx].len,
    )
    .expect("expected dominant side tick range");
    let (_, dyn_start_tick, dyn_end_tick) = section_tick_range_from_refs(
        &trace,
        &dynamics_targets[dynamics_idx].refs,
        dynamics_targets[dynamics_idx].len,
    )
    .expect("expected dominant dynamics tick range");

    let events = extract_ym2612_state_events_from_timed_writes(&trace.ym_writes, 0, trace.end_tick)
        .expect("soundlog should extract YM2612 events from GHZ trace");
    let (_, dyn_start_sample, dyn_end_sample) = section_sample_range_from_refs(
        &trace,
        &dynamics_targets[dynamics_idx].refs,
        dynamics_targets[dynamics_idx].len,
    )
    .expect("expected dominant dynamics sample range");
    let (dyn_counts, _) =
        summarize_ym2612_tracked_events_in_sample_range(&events, dyn_start_sample, dyn_end_sample);

    let ch1_mask = 1u8 << 0;
    let ch4_mask = 1u8 << 3;
    let candidates = [
        ("current_default", None::<(u64, u64, u8)>),
        (
            "freeze_ch1_dom_dyn",
            Some((dyn_start_tick, dyn_end_tick, ch1_mask)),
        ),
        (
            "freeze_ch4_dom_dyn",
            Some((dyn_start_tick, dyn_end_tick, ch4_mask)),
        ),
        (
            "freeze_ch1_ch4_dom_dyn",
            Some((dyn_start_tick, dyn_end_tick, ch1_mask | ch4_mask)),
        ),
        (
            "freeze_ch1_dom_side",
            Some((side_start_tick, side_end_tick, ch1_mask)),
        ),
        (
            "freeze_ch4_dom_side",
            Some((side_start_tick, side_end_tick, ch4_mask)),
        ),
        (
            "freeze_ch1_ch4_dom_side",
            Some((side_start_tick, side_end_tick, ch1_mask | ch4_mask)),
        ),
        (
            "freeze_ch1_ch4_both_dom",
            Some((
                dyn_start_tick.min(side_start_tick),
                dyn_end_tick.max(side_end_tick),
                ch1_mask | ch4_mask,
            )),
        ),
    ];

    eprintln!(
        "=== GHZ dominant tone-change causality === dyn_ch1_tone_changes={} dyn_ch4_tone_changes={} ===",
        dyn_counts[0][ym2612_event_kind_index(Ym2612TrackedEventKind::ToneChange)],
        dyn_counts[3][ym2612_event_kind_index(Ym2612TrackedEventKind::ToneChange)],
    );
    for (name, window) in candidates {
        let ym_writes = if let Some((start_tick, end_tick, channel_mask)) = window {
            suppress_ym_frequency_writes_for_channels_between(
                &trace.ym_writes,
                start_tick,
                end_tick,
                channel_mask,
            )
        } else {
            trace.ym_writes.clone()
        };

        let mut renderer = CoreAudioRenderer::with_audio_output_config(current_default);
        let rendered_full =
            renderer.render_timed_writes(&ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];

        let Some(mono) =
            score_fixed_mono_consensus_candidate(name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(name, rendered, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;

        match window {
            Some((start_tick, end_tick, channel_mask)) => {
                let start_s = (samples_from_tick(start_tick) as f64
                    - trace.capture_start_sample as f64)
                    / SAMPLE_RATE as f64;
                let end_s = (samples_from_tick(end_tick) as f64
                    - trace.capture_start_sample as f64)
                    / SAMPLE_RATE as f64;
                eprintln!(
                    "{name}: window={start_s:.2}s..{end_s:.2}s mask=0x{channel_mask:02X} mono={:.4} side={:.4} dyn={:.4} combined={combined:.4}",
                    mono.final_score, side_cons.final_score, side_dyn.final_score
                );
            }
            None => {
                eprintln!(
                    "{name}: mono={:.4} side={:.4} dyn={:.4} combined={combined:.4}",
                    mono.final_score, side_cons.final_score, side_dyn.final_score
                );
            }
        }
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_dominant_key_causality() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let current_dynamics = analyze_fixed_sectioned_side_dynamics_candidate_sections(
        "current_default",
        anchor_rendered,
        &dynamics_targets,
    )
    .expect("expected current_default dynamics analysis");
    let dynamics_finals: Vec<f32> = current_dynamics
        .iter()
        .map(|section| section.final_score)
        .collect();
    let dynamics_weights: Vec<f32> = dynamics_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let (dynamics_idx, _) = dominant_section_impact_index(&dynamics_finals, &dynamics_weights)
        .expect("expected dominant dynamics section");

    let (_, dyn_start_tick, dyn_end_tick) = section_tick_range_from_refs(
        &trace,
        &dynamics_targets[dynamics_idx].refs,
        dynamics_targets[dynamics_idx].len,
    )
    .expect("expected dominant dynamics tick range");

    let events = extract_ym2612_state_events_from_timed_writes(&trace.ym_writes, 0, trace.end_tick)
        .expect("soundlog should extract YM2612 events from GHZ trace");
    let (_, dyn_start_sample, dyn_end_sample) = section_sample_range_from_refs(
        &trace,
        &dynamics_targets[dynamics_idx].refs,
        dynamics_targets[dynamics_idx].len,
    )
    .expect("expected dominant dynamics sample range");
    let (dyn_counts, _) =
        summarize_ym2612_tracked_events_in_sample_range(&events, dyn_start_sample, dyn_end_sample);

    let ch1_mask = 1u8 << 0;
    let ch4_mask = 1u8 << 3;
    let ch5_mask = 1u8 << 4;
    let candidates = [
        ("current_default", None::<u8>),
        ("freeze_ch1_keys_dom_dyn", Some(ch1_mask)),
        ("freeze_ch4_keys_dom_dyn", Some(ch4_mask)),
        ("freeze_ch5_keys_dom_dyn", Some(ch5_mask)),
        ("freeze_ch4_ch5_keys_dom_dyn", Some(ch4_mask | ch5_mask)),
        (
            "freeze_ch1_ch4_ch5_keys_dom_dyn",
            Some(ch1_mask | ch4_mask | ch5_mask),
        ),
    ];

    eprintln!(
        "=== GHZ dominant key causality === dyn_key_counts ch1={}/{} ch4={}/{} ch5={}/{} ===",
        dyn_counts[0][ym2612_event_kind_index(Ym2612TrackedEventKind::KeyOn)],
        dyn_counts[0][ym2612_event_kind_index(Ym2612TrackedEventKind::KeyOff)],
        dyn_counts[3][ym2612_event_kind_index(Ym2612TrackedEventKind::KeyOn)],
        dyn_counts[3][ym2612_event_kind_index(Ym2612TrackedEventKind::KeyOff)],
        dyn_counts[4][ym2612_event_kind_index(Ym2612TrackedEventKind::KeyOn)],
        dyn_counts[4][ym2612_event_kind_index(Ym2612TrackedEventKind::KeyOff)],
    );
    for (name, channel_mask) in candidates {
        let ym_writes = if let Some(channel_mask) = channel_mask {
            suppress_ym_key_writes_for_channels_between(
                &trace.ym_writes,
                dyn_start_tick,
                dyn_end_tick,
                channel_mask,
            )
        } else {
            trace.ym_writes.clone()
        };

        let mut renderer = CoreAudioRenderer::with_audio_output_config(current_default);
        let rendered_full =
            renderer.render_timed_writes(&ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];

        let Some(mono) =
            score_fixed_mono_consensus_candidate(name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(name, rendered, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;

        match channel_mask {
            Some(channel_mask) => eprintln!(
                "{name}: window=28.79s..39.27s mask=0x{channel_mask:02X} mono={:.4} side={:.4} dyn={:.4} combined={combined:.4}",
                mono.final_score, side_cons.final_score, side_dyn.final_score
            ),
            None => eprintln!(
                "{name}: mono={:.4} side={:.4} dyn={:.4} combined={combined:.4}",
                mono.final_score, side_cons.final_score, side_dyn.final_score
            ),
        }
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_dominant_key_delay_sweep() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let current_dynamics = analyze_fixed_sectioned_side_dynamics_candidate_sections(
        "current_default",
        anchor_rendered,
        &dynamics_targets,
    )
    .expect("expected current_default dynamics analysis");
    let dynamics_finals: Vec<f32> = current_dynamics
        .iter()
        .map(|section| section.final_score)
        .collect();
    let dynamics_weights: Vec<f32> = dynamics_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let (dynamics_idx, dynamics_impact) =
        dominant_section_impact_index(&dynamics_finals, &dynamics_weights)
            .expect("expected dominant dynamics section");

    let (_, dyn_start_tick, dyn_end_tick) = section_tick_range_from_refs(
        &trace,
        &dynamics_targets[dynamics_idx].refs,
        dynamics_targets[dynamics_idx].len,
    )
    .expect("expected dominant dynamics tick range");

    let ch1_mask = 1u8 << 0;
    let ch4_mask = 1u8 << 3;
    let ch5_mask = 1u8 << 4;
    let candidates = [
        ("current_default", 0.0f32, 0u8),
        ("ch4", 1.0, ch4_mask),
        ("ch4", 2.0, ch4_mask),
        ("ch4", 4.0, ch4_mask),
        ("ch4", 6.0, ch4_mask),
        ("ch5", 1.0, ch5_mask),
        ("ch5", 2.0, ch5_mask),
        ("ch5", 4.0, ch5_mask),
        ("ch5", 6.0, ch5_mask),
        ("ch4_ch5", 1.0, ch4_mask | ch5_mask),
        ("ch4_ch5", 2.0, ch4_mask | ch5_mask),
        ("ch4_ch5", 4.0, ch4_mask | ch5_mask),
        ("ch4_ch5", 6.0, ch4_mask | ch5_mask),
        ("ch1_ch4_ch5", 2.0, ch1_mask | ch4_mask | ch5_mask),
        ("ch1_ch4_ch5", 4.0, ch1_mask | ch4_mask | ch5_mask),
    ];

    eprintln!(
        "=== GHZ dominant key delay sweep === dyn_window=28.79s..39.27s impact={dynamics_impact:.4}"
    );
    for (name, delay_ms, channel_mask) in candidates {
        let ym_writes = if channel_mask == 0 {
            trace.ym_writes.clone()
        } else {
            let delay_ticks =
                ((delay_ms as f64 / 1000.0) * MASTER_CLOCK_NTSC as f64).round() as u64;
            let delayed =
                delay_key_writes_for_channels(&trace.ym_writes, delay_ticks, channel_mask);
            let mut prefix = Vec::with_capacity(delayed.len());
            let mut suffix = Vec::with_capacity(delayed.len());
            for write in delayed {
                if write.master_tick >= dyn_start_tick && write.master_tick < dyn_end_tick {
                    suffix.push(write);
                } else {
                    prefix.push(write);
                }
            }
            prefix.extend(suffix);
            prefix.sort_by_key(|write| write.master_tick);
            prefix
        };

        let mut renderer = CoreAudioRenderer::with_audio_output_config(current_default);
        let rendered_full =
            renderer.render_timed_writes(&ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];

        let Some(mono) =
            score_fixed_mono_consensus_candidate(name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(name, rendered, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;

        if channel_mask == 0 {
            eprintln!(
                "{name}: mono={:.4} side={:.4} dyn={:.4} combined={combined:.4}",
                mono.final_score, side_cons.final_score, side_dyn.final_score
            );
        } else {
            eprintln!(
                "{name}: delay_ms={delay_ms:.1} mask=0x{channel_mask:02X} mono={:.4} side={:.4} dyn={:.4} combined={combined:.4}",
                mono.final_score, side_cons.final_score, side_dyn.final_score
            );
        }
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_dominant_key_delay_refine() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let current_dynamics = analyze_fixed_sectioned_side_dynamics_candidate_sections(
        "current_default",
        anchor_rendered,
        &dynamics_targets,
    )
    .expect("expected current_default dynamics analysis");
    let dynamics_finals: Vec<f32> = current_dynamics
        .iter()
        .map(|section| section.final_score)
        .collect();
    let dynamics_weights: Vec<f32> = dynamics_targets
        .iter()
        .map(|section| section.weight)
        .collect();
    let (dynamics_idx, dynamics_impact) =
        dominant_section_impact_index(&dynamics_finals, &dynamics_weights)
            .expect("expected dominant dynamics section");

    let (_, _dyn_start_tick, _dyn_end_tick) = section_tick_range_from_refs(
        &trace,
        &dynamics_targets[dynamics_idx].refs,
        dynamics_targets[dynamics_idx].len,
    )
    .expect("expected dominant dynamics tick range");

    let ch1_mask = 1u8 << 0;
    let ch4_mask = 1u8 << 3;
    let ch5_mask = 1u8 << 4;
    let candidates = [
        ("current_default", 0.0f32, 0u8),
        ("ch1_ch4_ch5", 3.0, ch1_mask | ch4_mask | ch5_mask),
        ("ch1_ch4_ch5", 3.5, ch1_mask | ch4_mask | ch5_mask),
        ("ch1_ch4_ch5", 4.0, ch1_mask | ch4_mask | ch5_mask),
        ("ch1_ch4_ch5", 4.5, ch1_mask | ch4_mask | ch5_mask),
        ("ch1_ch4_ch5", 5.0, ch1_mask | ch4_mask | ch5_mask),
        ("ch1_ch5", 4.0, ch1_mask | ch5_mask),
        ("ch1_ch4", 4.0, ch1_mask | ch4_mask),
        ("ch4_ch5", 4.0, ch4_mask | ch5_mask),
    ];

    eprintln!(
        "=== GHZ dominant key delay refine === dyn_window=28.79s..39.27s impact={dynamics_impact:.4}"
    );
    for (name, delay_ms, channel_mask) in candidates {
        let ym_writes = if channel_mask == 0 {
            trace.ym_writes.clone()
        } else {
            let delay_ticks =
                ((delay_ms as f64 / 1000.0) * MASTER_CLOCK_NTSC as f64).round() as u64;
            delay_key_writes_for_channels(&trace.ym_writes, delay_ticks, channel_mask)
        };

        let mut renderer = CoreAudioRenderer::with_audio_output_config(current_default);
        let rendered_full =
            renderer.render_timed_writes(&ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];

        let Some(mono) =
            score_fixed_mono_consensus_candidate(name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(name, rendered, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;

        if channel_mask == 0 {
            eprintln!(
                "{name}: mono={:.4} side={:.4} dyn={:.4} combined={combined:.4}",
                mono.final_score, side_cons.final_score, side_dyn.final_score
            );
        } else {
            eprintln!(
                "{name}: delay_ms={delay_ms:.1} mask=0x{channel_mask:02X} mono={:.4} side={:.4} dyn={:.4} combined={combined:.4}",
                mono.final_score, side_cons.final_score, side_dyn.final_score
            );
        }
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_ch4_tone_change_causality() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let events = extract_ym2612_state_events_from_timed_writes(&trace.ym_writes, 0, trace.end_tick)
        .expect("soundlog should extract YM2612 events from GHZ trace");
    let ch4_mask = 1 << 3;
    let windows = [
        ("current_default", None::<(u64, u64)>),
        (
            "freeze_ch4_freq_6_7",
            Some((
                trace.capture_start_tick
                    + master_ticks_from_output_samples(6 * SAMPLE_RATE as usize),
                trace.capture_start_tick
                    + master_ticks_from_output_samples(7 * SAMPLE_RATE as usize),
            )),
        ),
        (
            "freeze_ch4_freq_7_8",
            Some((
                trace.capture_start_tick
                    + master_ticks_from_output_samples(7 * SAMPLE_RATE as usize),
                trace.capture_start_tick
                    + master_ticks_from_output_samples(8 * SAMPLE_RATE as usize),
            )),
        ),
        (
            "freeze_ch4_freq_6_8",
            Some((
                trace.capture_start_tick
                    + master_ticks_from_output_samples(6 * SAMPLE_RATE as usize),
                trace.capture_start_tick
                    + master_ticks_from_output_samples(8 * SAMPLE_RATE as usize),
            )),
        ),
    ];

    eprintln!("=== GHZ ch4 tone-change causality ===");
    for (name, window) in windows {
        let ym_writes = if let Some((start_tick, end_tick)) = window {
            suppress_ym_frequency_writes_for_channels_between(
                &trace.ym_writes,
                start_tick,
                end_tick,
                ch4_mask,
            )
        } else {
            trace.ym_writes.clone()
        };

        let tone_changes_in_window = window.map_or(0usize, |(start_tick, end_tick)| {
            events
                .iter()
                .filter(|event| {
                    event.channel == 3
                        && event.kind == Ym2612TrackedEventKind::ToneChange
                        && u64::from(event.sample) >= samples_from_tick(start_tick)
                        && u64::from(event.sample) < samples_from_tick(end_tick)
                })
                .count()
        });

        let mut renderer = CoreAudioRenderer::with_audio_output_config(current_default);
        let rendered_full =
            renderer.render_timed_writes(&ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];

        let Some(mono) =
            score_fixed_mono_consensus_candidate(name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(name, rendered, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;

        match window {
            Some((start_tick, end_tick)) => {
                let start_s = (samples_from_tick(start_tick) as f64
                    - trace.capture_start_sample as f64)
                    / SAMPLE_RATE as f64;
                let end_s = (samples_from_tick(end_tick) as f64
                    - trace.capture_start_sample as f64)
                    / SAMPLE_RATE as f64;
                eprintln!(
                    "{name}: window={start_s:.2}s..{end_s:.2}s ch4_tone_changes={tone_changes_in_window} mono={:.4} side={:.4} dyn={:.4} combined={combined:.4}",
                    mono.final_score, side_cons.final_score, side_dyn.final_score
                );
            }
            None => {
                eprintln!(
                    "{name}: mono={:.4} side={:.4} dyn={:.4} combined={combined:.4}",
                    mono.final_score, side_cons.final_score, side_dyn.final_score
                );
            }
        }
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_ch4_windowed_pan_causality() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let ch4_mask = 1 << 3;
    let windows = [
        ("current_default", None::<(u64, u64)>),
        (
            "center_ch4_pan_6_7",
            Some((
                trace.capture_start_tick
                    + master_ticks_from_output_samples(6 * SAMPLE_RATE as usize),
                trace.capture_start_tick
                    + master_ticks_from_output_samples(7 * SAMPLE_RATE as usize),
            )),
        ),
        (
            "center_ch4_pan_7_8",
            Some((
                trace.capture_start_tick
                    + master_ticks_from_output_samples(7 * SAMPLE_RATE as usize),
                trace.capture_start_tick
                    + master_ticks_from_output_samples(8 * SAMPLE_RATE as usize),
            )),
        ),
        (
            "center_ch4_pan_6_8",
            Some((
                trace.capture_start_tick
                    + master_ticks_from_output_samples(6 * SAMPLE_RATE as usize),
                trace.capture_start_tick
                    + master_ticks_from_output_samples(8 * SAMPLE_RATE as usize),
            )),
        ),
    ];

    eprintln!("=== GHZ ch4 pan-window causality ===");
    for (name, window) in windows {
        let ym_writes = if let Some((start_tick, end_tick)) = window {
            force_centered_pan_writes_for_channels_between(
                &trace.ym_writes,
                start_tick,
                end_tick,
                ch4_mask,
            )
        } else {
            trace.ym_writes.clone()
        };

        let mut renderer = CoreAudioRenderer::with_audio_output_config(current_default);
        let rendered_full =
            renderer.render_timed_writes(&ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];

        let Some(mono) =
            score_fixed_mono_consensus_candidate(name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(name, rendered, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;

        match window {
            Some((start_tick, end_tick)) => {
                let start_s = (samples_from_tick(start_tick) as f64
                    - trace.capture_start_sample as f64)
                    / SAMPLE_RATE as f64;
                let end_s = (samples_from_tick(end_tick) as f64
                    - trace.capture_start_sample as f64)
                    / SAMPLE_RATE as f64;
                eprintln!(
                    "{name}: window={start_s:.2}s..{end_s:.2}s mono={:.4} side={:.4} dyn={:.4} combined={combined:.4}",
                    mono.final_score, side_cons.final_score, side_dyn.final_score
                );
            }
            None => {
                eprintln!(
                    "{name}: mono={:.4} side={:.4} dyn={:.4} combined={combined:.4}",
                    mono.final_score, side_cons.final_score, side_dyn.final_score
                );
            }
        }
    }
}

fn samples_from_tick(master_tick: u64) -> u64 {
    ((u128::from(master_tick) * u128::from(SAMPLE_RATE) + u128::from(MASTER_CLOCK_NTSC / 2))
        / u128::from(MASTER_CLOCK_NTSC)) as u64
}

/// Compare emulator output against a reference, reporting similarity metrics.
///
/// Returns (correlation, rms_emu, rms_ref) for the overlapping portion.
fn compare_audio(emu: &[f32], reference: &[f32]) -> (f32, f32, f32) {
    let n = emu.len().min(reference.len());
    let corr = cross_correlation(&emu[..n], &reference[..n]);
    let rms_e = rms(&emu[..n]);
    let rms_r = rms(&reference[..n]);
    (corr, rms_e, rms_r)
}

/// Compares emulator audio against a reference FLAC/WAV.
///
/// The reference file should be placed at:
///   tests/reference_audio/sonic_ghz.flac (or .wav)
///
/// To generate the emulator's baseline WAV for comparison:
///   cargo run -p genesoxide-desktop -- dump-audio <sonic_rom> \
///     --skip 320 --frames 600 --output sonic_ghz_emu.wav
#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_core_ym_channel_side_memory_candidates() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let candidates = [
        ("current_default", current_default),
        (
            "ym_side_mem_ch1_020_ch4_015",
            current_default.with_ym_channel_side_memory_amounts([0.20, 0.0, 0.0, 0.15, 0.0, 0.0]),
        ),
        (
            "ym_side_mem_ch1_020_ch4_020",
            current_default.with_ym_channel_side_memory_amounts([0.20, 0.0, 0.0, 0.20, 0.0, 0.0]),
        ),
        (
            "ym_side_mem_ch1_020_ch4_020_ch5_010",
            current_default.with_ym_channel_side_memory_amounts([0.20, 0.0, 0.0, 0.20, 0.10, 0.0]),
        ),
    ];
    let mut results = Vec::new();

    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];
        let Some(mono) =
            score_fixed_mono_consensus_candidate(name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(name, rendered, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name.to_owned(),
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.sort_by(|a, b| b.4.partial_cmp(&a.4).unwrap_or(std::cmp::Ordering::Equal));

    eprintln!("=== GHZ core YM channel side-memory candidates ===");
    for (name, mono_final, side_final, dyn_final, combined) in results {
        eprintln!(
            "{name}: mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_core_ym_channel_side_memory_refine() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let candidates = [
        ("current_default", current_default),
        (
            "ch1_015_ch4_010",
            current_default.with_ym_channel_side_memory_amounts([0.15, 0.0, 0.0, 0.10, 0.0, 0.0]),
        ),
        (
            "ch1_015_ch4_015",
            current_default.with_ym_channel_side_memory_amounts([0.15, 0.0, 0.0, 0.15, 0.0, 0.0]),
        ),
        (
            "ch1_020_ch4_010",
            current_default.with_ym_channel_side_memory_amounts([0.20, 0.0, 0.0, 0.10, 0.0, 0.0]),
        ),
        (
            "ch1_020_ch4_015",
            current_default.with_ym_channel_side_memory_amounts([0.20, 0.0, 0.0, 0.15, 0.0, 0.0]),
        ),
        (
            "ch1_020_ch4_020",
            current_default.with_ym_channel_side_memory_amounts([0.20, 0.0, 0.0, 0.20, 0.0, 0.0]),
        ),
        (
            "ch1_025_ch4_010",
            current_default.with_ym_channel_side_memory_amounts([0.25, 0.0, 0.0, 0.10, 0.0, 0.0]),
        ),
        (
            "ch1_025_ch4_015",
            current_default.with_ym_channel_side_memory_amounts([0.25, 0.0, 0.0, 0.15, 0.0, 0.0]),
        ),
        (
            "ch1_025_ch4_020",
            current_default.with_ym_channel_side_memory_amounts([0.25, 0.0, 0.0, 0.20, 0.0, 0.0]),
        ),
    ];
    let mut results = Vec::new();

    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];
        let Some(mono) =
            score_fixed_mono_consensus_candidate(name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(name, rendered, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name.to_owned(),
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.sort_by(|a, b| b.4.partial_cmp(&a.4).unwrap_or(std::cmp::Ordering::Equal));

    eprintln!("=== GHZ core YM channel side-memory refine ===");
    for (name, mono_final, side_final, dyn_final, combined) in results {
        eprintln!(
            "{name}: mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_core_ym_channel_side_memory_ch45_refine() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let mut results = Vec::new();
    for &(ch1, ch4, ch5) in &[
        (0.20f32, 0.15f32, 0.00f32),
        (0.20f32, 0.20f32, 0.05f32),
        (0.20f32, 0.20f32, 0.10f32),
        (0.20f32, 0.20f32, 0.15f32),
        (0.20f32, 0.25f32, 0.05f32),
        (0.20f32, 0.25f32, 0.10f32),
        (0.20f32, 0.25f32, 0.15f32),
        (0.25f32, 0.20f32, 0.05f32),
        (0.25f32, 0.20f32, 0.10f32),
        (0.25f32, 0.25f32, 0.05f32),
        (0.25f32, 0.25f32, 0.10f32),
        (0.25f32, 0.25f32, 0.15f32),
    ] {
        let config =
            current_default.with_ym_channel_side_memory_amounts([ch1, 0.0, 0.0, ch4, ch5, 0.0]);
        let name = format!("ch1_{ch1:.2}_ch4_{ch4:.2}_ch5_{ch5:.2}");
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];
        let Some(mono) =
            score_fixed_mono_consensus_candidate(&name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(&name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(&name, rendered, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name,
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.push((
        "current_default".to_owned(),
        score_fixed_mono_consensus_candidate(
            "current_default",
            anchor_rendered,
            &mono_refs,
            &mono_target,
        )
        .expect("expected mono score")
        .final_score,
        score_fixed_sectioned_side_consensus_candidate(
            "current_default",
            anchor_rendered,
            &side_targets,
        )
        .expect("expected side score")
        .final_score,
        score_fixed_sectioned_side_dynamics_candidate(
            "current_default",
            anchor_rendered,
            &dynamics_targets,
        )
        .expect("expected dynamics score")
        .final_score,
        0.0,
    ));
    if let Some(last) = results.last_mut() {
        last.4 = last.1 * 0.70 + last.2 * 0.20 + last.3 * 0.10;
    }

    results.sort_by(|a, b| {
        b.3.partial_cmp(&a.3)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ core YM channel side-memory ch4/ch5 refine ===");
    for (name, mono_final, side_final, dyn_final, combined) in results {
        eprintln!(
            "{name}: mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_side_memory_width_refine() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let base =
        current_default.with_ym_channel_side_memory_amounts([0.20, 0.0, 0.0, 0.25, 0.15, 0.0]);
    let mut results = Vec::new();

    for crossfeed in [0.10f32, 0.15, 0.20, 0.25, 0.30, 0.35, 0.40, 0.45] {
        for side_gain in [0.95f32, 1.00, 1.05, 1.10, 1.15] {
            let config = base
                .with_stereo_crossfeed(crossfeed)
                .with_side_gain(side_gain);
            let name = format!("xf_{crossfeed:.2}_side_{side_gain:.2}");
            let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
            let rendered_full = renderer.render_timed_writes(
                &trace.ym_writes,
                &trace.psg_writes,
                0,
                trace.end_tick,
            );
            let rendered = &rendered_full[capture_start..];
            let Some(mono) =
                score_fixed_mono_consensus_candidate(&name, rendered, &mono_refs, &mono_target)
            else {
                continue;
            };
            let Some(side_cons) =
                score_fixed_sectioned_side_consensus_candidate(&name, rendered, &side_targets)
            else {
                continue;
            };
            let Some(side_dyn) =
                score_fixed_sectioned_side_dynamics_candidate(&name, rendered, &dynamics_targets)
            else {
                continue;
            };
            let combined = mono.final_score * 0.70
                + side_cons.final_score * 0.20
                + side_dyn.final_score * 0.10;
            results.push((
                name,
                crossfeed,
                side_gain,
                mono.final_score,
                side_cons.final_score,
                side_dyn.final_score,
                combined,
            ));
        }
    }

    results.push((
        "current_default".to_owned(),
        current_default.stereo_crossfeed,
        current_default.side_gain,
        score_fixed_mono_consensus_candidate(
            "current_default",
            anchor_rendered,
            &mono_refs,
            &mono_target,
        )
        .expect("expected mono score")
        .final_score,
        score_fixed_sectioned_side_consensus_candidate(
            "current_default",
            anchor_rendered,
            &side_targets,
        )
        .expect("expected side score")
        .final_score,
        score_fixed_sectioned_side_dynamics_candidate(
            "current_default",
            anchor_rendered,
            &dynamics_targets,
        )
        .expect("expected dynamics score")
        .final_score,
        0.0,
    ));
    if let Some(last) = results.last_mut() {
        last.6 = last.3 * 0.70 + last.4 * 0.20 + last.5 * 0.10;
    }

    results.sort_by(|a, b| {
        b.5.partial_cmp(&a.5)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.4.partial_cmp(&a.4).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ side-memory width refine ===");
    for (name, crossfeed, side_gain, mono_final, side_final, dyn_final, combined) in results {
        eprintln!(
            "{name}: xf={crossfeed:.2} side_gain={side_gain:.2} mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_side_memory_strength_refine() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let mut results = Vec::new();
    for &(ch1, ch4, ch5) in &[
        (0.20f32, 0.25f32, 0.10f32),
        (0.20f32, 0.25f32, 0.15f32),
        (0.20f32, 0.25f32, 0.20f32),
        (0.20f32, 0.25f32, 0.25f32),
        (0.20f32, 0.30f32, 0.10f32),
        (0.20f32, 0.30f32, 0.15f32),
        (0.20f32, 0.30f32, 0.20f32),
        (0.20f32, 0.30f32, 0.25f32),
        (0.20f32, 0.35f32, 0.10f32),
        (0.20f32, 0.35f32, 0.15f32),
        (0.20f32, 0.35f32, 0.20f32),
        (0.20f32, 0.35f32, 0.25f32),
        (0.25f32, 0.30f32, 0.15f32),
        (0.25f32, 0.30f32, 0.20f32),
        (0.25f32, 0.35f32, 0.15f32),
        (0.25f32, 0.35f32, 0.20f32),
    ] {
        let config = current_default
            .with_ym_channel_side_memory_amounts([ch1, 0.0, 0.0, ch4, ch5, 0.0])
            .with_stereo_crossfeed(0.25)
            .with_side_gain(1.05);
        let name = format!("ch1_{ch1:.2}_ch4_{ch4:.2}_ch5_{ch5:.2}");
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];
        let Some(mono) =
            score_fixed_mono_consensus_candidate(&name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(&name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(&name, rendered, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name,
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.push((
        "current_default".to_owned(),
        score_fixed_mono_consensus_candidate(
            "current_default",
            anchor_rendered,
            &mono_refs,
            &mono_target,
        )
        .expect("expected mono score")
        .final_score,
        score_fixed_sectioned_side_consensus_candidate(
            "current_default",
            anchor_rendered,
            &side_targets,
        )
        .expect("expected side score")
        .final_score,
        score_fixed_sectioned_side_dynamics_candidate(
            "current_default",
            anchor_rendered,
            &dynamics_targets,
        )
        .expect("expected dynamics score")
        .final_score,
        0.0,
    ));
    if let Some(last) = results.last_mut() {
        last.4 = last.1 * 0.70 + last.2 * 0.20 + last.3 * 0.10;
    }

    results.sort_by(|a, b| {
        b.3.partial_cmp(&a.3)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ side-memory strength refine ===");
    for (name, mono_final, side_final, dyn_final, combined) in results {
        eprintln!(
            "{name}: mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_core_ym_channel_key_delay_candidates() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let candidates = [
        ("current_default", current_default),
        (
            "key_delay_ch1_ch4_ch5_3p5",
            current_default.with_ym_channel_key_delay_ms([3.5, 0.0, 0.0, 3.5, 3.5, 0.0]),
        ),
        (
            "key_delay_ch1_ch4_ch5_4p0",
            current_default.with_ym_channel_key_delay_ms([4.0, 0.0, 0.0, 4.0, 4.0, 0.0]),
        ),
        (
            "key_delay_ch1_ch4_ch5_4p5",
            current_default.with_ym_channel_key_delay_ms([4.5, 0.0, 0.0, 4.5, 4.5, 0.0]),
        ),
        (
            "key_delay_ch1_ch4_ch5_5p0",
            current_default.with_ym_channel_key_delay_ms([5.0, 0.0, 0.0, 5.0, 5.0, 0.0]),
        ),
        (
            "key_delay_ch1_ch5_5p0",
            current_default.with_ym_channel_key_delay_ms([5.0, 0.0, 0.0, 0.0, 5.0, 0.0]),
        ),
        (
            "key_delay_ch4_ch5_4p0",
            current_default.with_ym_channel_key_delay_ms([0.0, 0.0, 0.0, 4.0, 4.0, 0.0]),
        ),
    ];
    let mut results = Vec::new();

    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];
        let Some(mono) =
            score_fixed_mono_consensus_candidate(name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(name, rendered, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name.to_owned(),
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.sort_by(|a, b| b.4.partial_cmp(&a.4).unwrap_or(std::cmp::Ordering::Equal));

    eprintln!("=== GHZ core YM key-delay candidates ===");
    for (name, mono_final, side_final, dyn_final, combined) in results {
        eprintln!(
            "{name}: mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_reference_authority_and_ablation() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.len() < 3 {
        eprintln!("Need at least three GHZ references for ablation, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 3 {
        eprintln!("Need at least three 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let mut renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let rendered_full =
        renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(rendered_full.len());
    let rendered = &rendered_full[capture_start..];

    let Some((_mono_target, mono_refs)) = build_fixed_mono_consensus_target(rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };
    let Some(baseline) = score_fixed_oracle_snapshot(rendered, &loaded_refs) else {
        eprintln!("Unable to score baseline oracle snapshot");
        return;
    };

    eprintln!("=== GHZ reference authority ===");
    for summary in
        summarize_reference_authority(&loaded_refs, &mono_refs, &side_targets, &dynamics_targets)
    {
        eprintln!(
            "{}: mono manual={:.2} effective={:.4} side manual={:.2} side_cons={:.4} side_dyn={:.4}",
            summary.name,
            summary.mono_manual_weight,
            summary.mono_effective_weight,
            summary.side_manual_weight,
            summary.side_consensus_effective_weight,
            summary.side_dynamics_effective_weight,
        );
    }
    eprintln!(
        "baseline: mono={:.4} hybrid={:.4} side={:.4} dyn={:.4}",
        baseline.mono_final,
        baseline.hybrid_final,
        baseline.side_consensus_final,
        baseline.side_dynamics_final,
    );

    eprintln!("=== GHZ reference ablation ===");
    for drop_idx in 0..loaded_refs.len() {
        let dropped_name = loaded_refs[drop_idx]
            .0
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_owned();
        let subset: Vec<_> = loaded_refs
            .iter()
            .enumerate()
            .filter(|(idx, _)| *idx != drop_idx)
            .map(|(_, (path, samples))| (path.clone(), samples.clone()))
            .collect();
        let Some(snapshot) = score_fixed_oracle_snapshot(rendered, &subset) else {
            eprintln!("drop {dropped_name}: unable to score subset");
            continue;
        };

        eprintln!(
            "drop {dropped_name}: mono={:.4} ({:+.4}) hybrid={:.4} ({:+.4}) side={:.4} ({:+.4}) dyn={:.4} ({:+.4})",
            snapshot.mono_final,
            snapshot.mono_final - baseline.mono_final,
            snapshot.hybrid_final,
            snapshot.hybrid_final - baseline.hybrid_final,
            snapshot.side_consensus_final,
            snapshot.side_consensus_final - baseline.side_consensus_final,
            snapshot.side_dynamics_final,
            snapshot.side_dynamics_final - baseline.side_dynamics_final,
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_16bap_side_weight_sweep() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.len() < 3 {
        eprintln!("Need at least three GHZ references for side-weight sweep, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 3 {
        eprintln!("Need at least three 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();
    let mut renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let rendered_full =
        renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(rendered_full.len());
    let rendered = &rendered_full[capture_start..];

    let target_filename = "sonic_ghz_16bap.flac".to_owned();
    let mut results = Vec::new();
    for side_weight in [0.0f32, 0.15, 0.30, 0.45, 0.60, 0.65, 0.75, 0.90, 1.0] {
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert(target_filename.clone(), side_weight);
        let Some(snapshot) =
            score_fixed_oracle_snapshot_with_weight_resolver(rendered, &loaded_refs, |path| {
                ghz_reference_weight_with_side_overrides(path, &overrides)
            })
        else {
            continue;
        };
        results.push((
            side_weight,
            snapshot.mono_final,
            snapshot.hybrid_final,
            snapshot.side_consensus_final,
            snapshot.side_dynamics_final,
            snapshot.hybrid_final * 0.70
                + snapshot.side_consensus_final * 0.15
                + snapshot.side_dynamics_final * 0.15,
        ));
    }

    results.sort_by(|a, b| {
        b.4.partial_cmp(&a.4)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ 16bap side-weight sweep ===");
    for (side_weight, mono, hybrid, side, dyn_score, combined) in results {
        eprintln!(
            "16bap_side={side_weight:.2}: mono={mono:.4} hybrid={hybrid:.4} side={side:.4} dyn={dyn_score:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "requires a local commercial Sonic ROM (see SONIC_ROM_PATH) + GHZ reference audio (sonic_ghz.flac/.wav) in tests/reference_audio/; run with -- --ignored"]
fn sonic_ghz_audio_comparison() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!(
                "[genesoxide][audio_golden] Sonic ROM not found at SONIC_ROM_PATH — \
                 place a commercial Sonic ROM there and run with -- --ignored to exercise this test."
            );
            return;
        }
    };

    // Look for reference file (try FLAC first, then WAV)
    let ref_dir = Path::new(REFERENCE_DIR);
    let ref_path = ["sonic_ghz.flac", "sonic_ghz.wav"]
        .iter()
        .map(|f| ref_dir.join(f))
        .find(|p| p.exists());

    let ref_path = match ref_path {
        Some(p) => p,
        None => {
            eprintln!(
                "[genesoxide][audio_golden] No GHZ reference audio in {REFERENCE_DIR} — \
                 place sonic_ghz.flac or sonic_ghz.wav there to enable this comparison."
            );
            return;
        }
    };

    eprintln!("Loading reference: {}", ref_path.display());
    let (ref_rate, ref_samples) = load_reference(&ref_path).expect("Failed to load reference");
    eprintln!(
        "Reference: {} samples, {} Hz, {:.1}s",
        ref_samples.len(),
        ref_rate,
        ref_samples.len() as f64 / (2.0 * ref_rate as f64)
    );

    // Generate emulator audio for the same duration after actually entering
    // gameplay. GHZ does not start by magic; someone has to press Start.
    let record_frames = (ref_samples.len() as f64 / (2.0 * SAMPLE_RATE as f64) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    eprintln!("Generating {record_frames} frames of emulator audio...");
    let emu_samples = generate_ghz_audio(&rom, record_frames);
    eprintln!("Emulator: {} samples", emu_samples.len());

    // Compare left channels only (every other sample starting at 0)
    let emu_left: Vec<f32> = emu_samples.iter().step_by(2).copied().collect();
    let ref_left: Vec<f32> = ref_samples.iter().step_by(2).copied().collect();

    let (corr, rms_e, rms_r) = compare_audio(&emu_left, &ref_left);
    let env_window = 2048;
    let emu_env = rms_envelope(&emu_left, env_window);
    let ref_env = rms_envelope(&ref_left, env_window);
    let max_lag_windows = emu_env.len().min(ref_env.len()).saturating_sub(64);
    let (env_corr, env_lag) = best_lagged_correlation(&emu_env, &ref_env, max_lag_windows);
    let env_lag_secs = env_lag as f64 * env_window as f64 / SAMPLE_RATE as f64;
    let local_env_window = (SAMPLE_RATE as usize * 8 / env_window).max(64);
    let local_match = best_windowed_match_at_lag(&emu_env, &ref_env, env_lag, local_env_window)
        .expect("expected a usable local GHZ envelope match");
    let local_env_corr = local_match.corr;
    let local_env_secs = local_match.len as f64 * env_window as f64 / SAMPLE_RATE as f64;
    let local_emu_start = local_match.a_start * env_window;
    let local_ref_start = local_match.b_start * env_window;
    let local_raw_len = (local_match.len * env_window)
        .min(emu_left.len().saturating_sub(local_emu_start))
        .min(ref_left.len().saturating_sub(local_ref_start));
    let local_raw_corr = cross_correlation(
        &emu_left[local_emu_start..local_emu_start + local_raw_len],
        &ref_left[local_ref_start..local_ref_start + local_raw_len],
    );
    let spectral_bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 12_000.0, 24);
    let local_spectral = spectral_similarity(
        &emu_left[local_emu_start..local_emu_start + local_raw_len],
        &ref_left[local_ref_start..local_ref_start + local_raw_len],
        2048,
        1024,
        &spectral_bins,
    );
    let local_rms_e = rms(&emu_left[local_emu_start..local_emu_start + local_raw_len]);
    let local_rms_r = rms(&ref_left[local_ref_start..local_ref_start + local_raw_len]);
    let local_rms_ratio = if local_rms_r > 1e-6 {
        local_rms_e / local_rms_r
    } else {
        0.0
    };
    let emu_stereo_local = stereo_window(&emu_samples, local_emu_start, local_raw_len);
    let ref_stereo_local = stereo_window(&ref_samples, local_ref_start, local_raw_len);
    let emu_lr_corr = stereo_lr_correlation(emu_stereo_local);
    let ref_lr_corr = stereo_lr_correlation(ref_stereo_local);
    let emu_side_ratio = stereo_side_ratio(emu_stereo_local);
    let ref_side_ratio = stereo_side_ratio(ref_stereo_local);
    let (emu_mid, emu_side) = stereo_mid_side(emu_stereo_local);
    let (ref_mid, ref_side) = stereo_mid_side(ref_stereo_local);
    let emu_mid_spectral = spectral_similarity(&emu_mid, &ref_mid, 2048, 1024, &spectral_bins);
    let emu_side_spectral = spectral_similarity(&emu_side, &ref_side, 2048, 1024, &spectral_bins);
    let (emu_left_local, emu_right_local) = stereo_left_right(emu_stereo_local);
    let (ref_left_local, ref_right_local) = stereo_left_right(ref_stereo_local);
    let left_phase =
        average_phase_delta(&emu_left_local, &ref_left_local, 2048, 1024, &spectral_bins);
    let right_phase = average_phase_delta(
        &emu_right_local,
        &ref_right_local,
        2048,
        1024,
        &spectral_bins,
    );
    let mid_phase = average_phase_delta(&emu_mid, &ref_mid, 2048, 1024, &spectral_bins);
    let side_phase = average_phase_delta(&emu_side, &ref_side, 2048, 1024, &spectral_bins);
    let left_phase_coherence = mean_phase_coherence(&left_phase);
    let right_phase_coherence = mean_phase_coherence(&right_phase);
    let mid_phase_coherence = mean_phase_coherence(&mid_phase);
    let side_phase_coherence = mean_phase_coherence(&side_phase);
    let (emu_lr_lag_corr, emu_lr_lag) = stereo_interchannel_lag(emu_stereo_local, 4);
    let (ref_lr_lag_corr, ref_lr_lag) = stereo_interchannel_lag(ref_stereo_local, 4);
    let rms_ratio = if rms_r > 1e-6 { rms_e / rms_r } else { 0.0 };
    let loop_offset_bins = local_match.b_start.abs_diff(local_match.a_start);
    let loop_offset_samples = loop_offset_bins.saturating_mul(env_window);
    let lag_samples = env_lag * env_window as isize;
    let reference_self_match = if loop_offset_samples > 0 && local_ref_start >= loop_offset_samples
    {
        let prior_ref_start = local_ref_start - loop_offset_samples;
        let enough_samples = prior_ref_start
            .checked_add(local_raw_len)
            .is_some_and(|end| end <= ref_left.len());
        if enough_samples {
            let ref_prior_left = &ref_left[prior_ref_start..prior_ref_start + local_raw_len];
            let ref_current_left = &ref_left[local_ref_start..local_ref_start + local_raw_len];
            let ref_prior_stereo = stereo_window(&ref_samples, prior_ref_start, local_raw_len);
            let ref_current_stereo = stereo_window(&ref_samples, local_ref_start, local_raw_len);
            let (prior_mid, prior_side) = stereo_mid_side(ref_prior_stereo);
            let (current_mid, current_side) = stereo_mid_side(ref_current_stereo);
            let (prior_left, prior_right) = stereo_left_right(ref_prior_stereo);
            let (current_left, current_right) = stereo_left_right(ref_current_stereo);
            let left_phase =
                average_phase_delta(&prior_left, &current_left, 2048, 1024, &spectral_bins);
            let right_phase =
                average_phase_delta(&prior_right, &current_right, 2048, 1024, &spectral_bins);
            let mid_phase =
                average_phase_delta(&prior_mid, &current_mid, 2048, 1024, &spectral_bins);
            let side_phase =
                average_phase_delta(&prior_side, &current_side, 2048, 1024, &spectral_bins);
            Some((
                cross_correlation(ref_prior_left, ref_current_left),
                spectral_similarity(ref_prior_left, ref_current_left, 2048, 1024, &spectral_bins),
                rms(ref_current_left) / rms(ref_prior_left).max(1e-9),
                spectral_similarity(&prior_mid, &current_mid, 2048, 1024, &spectral_bins),
                spectral_similarity(&prior_side, &current_side, 2048, 1024, &spectral_bins),
                mean_phase_coherence(&left_phase),
                mean_phase_coherence(&right_phase),
                mean_phase_coherence(&mid_phase),
                mean_phase_coherence(&side_phase),
            ))
        } else {
            None
        }
    } else {
        None
    };
    let trusted_reference_env_match = if loop_offset_bins > 0 {
        best_windowed_match_at_lag(
            &ref_env,
            &ref_env,
            loop_offset_bins as isize,
            local_env_window,
        )
    } else {
        None
    };
    let trusted_reference_window = trusted_reference_env_match.and_then(|trusted_env| {
        let trusted_prior_start = trusted_env.b_start * env_window;
        let trusted_current_start = trusted_env.a_start * env_window;
        let trusted_raw_len = (trusted_env.len * env_window)
            .min(ref_left.len().saturating_sub(trusted_prior_start))
            .min(ref_left.len().saturating_sub(trusted_current_start));
        reference_self_window_at_offset(
            &ref_samples,
            trusted_prior_start,
            trusted_current_start,
            trusted_raw_len,
            2048,
            1024,
            &spectral_bins,
        )
    });
    let trusted_reference_match = trusted_reference_window.and_then(|trusted| {
        let trusted_emu_start = trusted.current_start as isize + lag_samples;
        if trusted_emu_start < 0 {
            return None;
        }
        let trusted_emu_start = trusted_emu_start as usize;
        let trusted_ref_start = trusted.current_start;
        let enough_samples = trusted_emu_start
            .checked_add(trusted.len)
            .is_some_and(|end| end <= emu_left.len())
            && trusted_ref_start
                .checked_add(trusted.len)
                .is_some_and(|end| end <= ref_left.len());
        if !enough_samples {
            return None;
        }

        let emu_window = &emu_left[trusted_emu_start..trusted_emu_start + trusted.len];
        let ref_window = &ref_left[trusted_ref_start..trusted_ref_start + trusted.len];
        let emu_stereo = stereo_window(&emu_samples, trusted_emu_start, trusted.len);
        let ref_stereo = stereo_window(&ref_samples, trusted_ref_start, trusted.len);
        let (emu_mid, emu_side) = stereo_mid_side(emu_stereo);
        let (ref_mid, ref_side) = stereo_mid_side(ref_stereo);

        Some((
            trusted,
            cross_correlation(emu_window, ref_window),
            spectral_similarity(emu_window, ref_window, 2048, 1024, &spectral_bins),
            rms(emu_window) / rms(ref_window).max(1e-9),
            spectral_similarity(&emu_mid, &ref_mid, 2048, 1024, &spectral_bins),
            spectral_similarity(&emu_side, &ref_side, 2048, 1024, &spectral_bins),
        ))
    });
    let mono_consensus_oracle = {
        let mut loaded_refs = Vec::new();
        for path in ghz_reference_paths() {
            let Some((rate, samples)) = load_reference(&path) else {
                continue;
            };
            if rate != SAMPLE_RATE {
                continue;
            }
            loaded_refs.push((path, samples));
        }
        build_fixed_mono_consensus_target(&emu_samples, &loaded_refs).and_then(
            |(target, fixed_refs)| {
                score_fixed_mono_consensus_candidate(
                    "current_default",
                    &emu_samples,
                    &fixed_refs,
                    &target,
                )
                .map(|score| (target, score, fixed_refs.len()))
            },
        )
    };
    let hybrid_mono_consensus_oracle = {
        let mut loaded_refs = Vec::new();
        for path in ghz_reference_paths() {
            let Some((rate, samples)) = load_reference(&path) else {
                continue;
            };
            if rate != SAMPLE_RATE {
                continue;
            }
            loaded_refs.push((path, samples));
        }
        build_fixed_mono_consensus_target(&emu_samples, &loaded_refs).and_then(
            |(target, fixed_refs)| {
                score_fixed_hybrid_mono_consensus_candidate(
                    "current_default",
                    &emu_samples,
                    &fixed_refs,
                    &loaded_refs,
                    &target,
                )
                .map(|score| (score, fixed_refs.len()))
            },
        )
    };
    let sectioned_side_consensus_oracle = {
        let mut loaded_refs = Vec::new();
        for path in ghz_reference_paths() {
            let Some((rate, samples)) = load_reference(&path) else {
                continue;
            };
            if rate != SAMPLE_RATE {
                continue;
            }
            loaded_refs.push((path, samples));
        }
        build_fixed_sectioned_side_consensus(&emu_samples, &loaded_refs, SAMPLE_RATE as usize)
            .and_then(|sections| {
                let score = score_fixed_sectioned_side_consensus_candidate(
                    "current_default",
                    &emu_samples,
                    &sections,
                )?;
                let analyzed = analyze_fixed_sectioned_side_consensus_candidate_sections(
                    "current_default",
                    &emu_samples,
                    &sections,
                )?;
                let section_finals: Vec<f32> =
                    analyzed.iter().map(|section| section.final_score).collect();
                let section_weights: Vec<f32> =
                    sections.iter().map(|section| section.weight).collect();
                let dominant = dominant_section_impact_index(&section_finals, &section_weights)
                    .map(|(idx, impact)| {
                        (
                            sections[idx].start,
                            sections[idx].len,
                            analyzed[idx].final_score,
                            impact,
                        )
                    })?;
                let weakest = analyzed
                    .iter()
                    .enumerate()
                    .min_by(|a, b| {
                        a.1.final_score
                            .partial_cmp(&b.1.final_score)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|(idx, section)| {
                        (sections[idx].start, sections[idx].len, section.final_score)
                    })?;
                Some((score, sections.len(), dominant, weakest))
            })
    };
    let sectioned_side_dynamics_oracle = {
        let mut loaded_refs = Vec::new();
        for path in ghz_reference_paths() {
            let Some((rate, samples)) = load_reference(&path) else {
                continue;
            };
            if rate != SAMPLE_RATE {
                continue;
            }
            loaded_refs.push((path, samples));
        }
        build_fixed_sectioned_side_dynamics(&emu_samples, &loaded_refs, SAMPLE_RATE as usize)
            .and_then(|sections| {
                let score = score_fixed_sectioned_side_dynamics_candidate(
                    "current_default",
                    &emu_samples,
                    &sections,
                )?;
                let analyzed = analyze_fixed_sectioned_side_dynamics_candidate_sections(
                    "current_default",
                    &emu_samples,
                    &sections,
                )?;
                let section_finals: Vec<f32> =
                    analyzed.iter().map(|section| section.final_score).collect();
                let section_weights: Vec<f32> =
                    sections.iter().map(|section| section.weight).collect();
                let dominant = dominant_section_impact_index(&section_finals, &section_weights)
                    .map(|(idx, impact)| {
                        (
                            sections[idx].start,
                            sections[idx].len,
                            analyzed[idx].final_score,
                            impact,
                        )
                    })?;
                let (weakest_idx, weakest) = analyzed
                    .iter()
                    .enumerate()
                    .min_by(|a, b| {
                        a.1.final_score
                            .partial_cmp(&b.1.final_score)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|(idx, section)| {
                        let target_transient =
                            envelope_transient_profile(&sections[idx].target.envelope);
                        let candidate_transient =
                            envelope_transient_profile(&section.candidate.envelope);
                        let (lag_bins, adjusted_env) = best_envelope_offset_bins(
                            &section.candidate.envelope,
                            &sections[idx].target.envelope,
                            SIDE_DYNAMICS_MAX_LAG_BINS,
                        )
                        .unwrap_or((
                            0,
                            cross_correlation(
                                &section.candidate.envelope,
                                &sections[idx].target.envelope,
                            ),
                        ));
                        let transient = cross_correlation(&candidate_transient, &target_transient);
                        let adjusted_transient = correlation_at_offset_bins(
                            &candidate_transient,
                            &target_transient,
                            lag_bins,
                        );
                        let transient_rms_ratio =
                            rms(&candidate_transient) / rms(&target_transient).max(1e-9);
                        (
                            idx,
                            (
                                sections[idx].start,
                                sections[idx].len,
                                section.final_score,
                                lag_bins,
                                adjusted_env,
                                transient,
                                adjusted_transient,
                                transient_rms_ratio,
                            ),
                        )
                    })?;
                let weakest_reference_transient =
                    analyze_reference_side_dynamics_pairs(&loaded_refs, &sections[weakest_idx])
                        .and_then(|pairs| summarize_reference_side_dynamics_pairs(&pairs));
                Some((
                    score,
                    sections.len(),
                    dominant,
                    weakest,
                    weakest_reference_transient,
                ))
            })
    };

    eprintln!("=== Audio Comparison ===");
    eprintln!("Cross-correlation: {corr:.4}");
    eprintln!("Envelope corr:    {env_corr:.4} @ lag {env_lag_secs:.2}s");
    eprintln!(
        "Best local env:   {local_env_corr:.4} over {local_env_secs:.2}s at env bins {}:{}",
        local_match.a_start, local_match.b_start
    );
    eprintln!("Local raw corr:   {local_raw_corr:.4}");
    eprintln!("Local spectral:   {local_spectral:.4}");
    eprintln!("RMS (emu):   {rms_e:.4}");
    eprintln!("RMS (ref):   {rms_r:.4}");
    eprintln!("RMS ratio:   {rms_ratio:.4} (1.0 = same level)");
    eprintln!("Local RMS ratio:  {local_rms_ratio:.4} (aligned matched window)");
    eprintln!(
        "Local stereo: emu_lr_corr={emu_lr_corr:.4} ref_lr_corr={ref_lr_corr:.4} emu_side={emu_side_ratio:.4} ref_side={ref_side_ratio:.4}"
    );
    eprintln!("Local mid/side spectral: mid={emu_mid_spectral:.4} side={emu_side_spectral:.4}");
    eprintln!(
        "Local phase coherence: left={left_phase_coherence:.4} right={right_phase_coherence:.4} mid={mid_phase_coherence:.4} side={side_phase_coherence:.4}"
    );
    eprintln!(
        "Local stereo lag: emu={emu_lr_lag:+} ({emu_lr_lag_corr:.4}) ref={ref_lr_lag:+} ({ref_lr_lag_corr:.4})"
    );
    if let Some((
        ref_self_raw_corr,
        ref_self_spectral,
        ref_self_rms_ratio,
        ref_self_mid_spectral,
        ref_self_side_spectral,
        ref_self_left_phase,
        ref_self_right_phase,
        ref_self_mid_phase,
        ref_self_side_phase,
    )) = reference_self_match
    {
        let loop_offset_secs = loop_offset_samples as f64 / SAMPLE_RATE as f64;
        eprintln!(
            "Reference self-match @ inferred loop {loop_offset_secs:.2}s: raw={ref_self_raw_corr:.4} spectral={ref_self_spectral:.4} rms={ref_self_rms_ratio:.4}"
        );
        eprintln!(
            "Reference self mid/side spectral: mid={ref_self_mid_spectral:.4} side={ref_self_side_spectral:.4}"
        );
        eprintln!(
            "Reference self phase coherence: left={ref_self_left_phase:.4} right={ref_self_right_phase:.4} mid={ref_self_mid_phase:.4} side={ref_self_side_phase:.4}"
        );
    }
    if let Some((
        trusted,
        trusted_raw_corr,
        trusted_spectral,
        trusted_rms_ratio,
        trusted_mid_spectral,
        trusted_side_spectral,
    )) = trusted_reference_match
    {
        eprintln!(
            "Trusted ref window: prior={:.2}s current={:.2}s score={:.4} self_left={:.4} self_mid={:.4} self_side={:.4} self_rms={:.4}",
            trusted.prior_start as f64 / SAMPLE_RATE as f64,
            trusted.current_start as f64 / SAMPLE_RATE as f64,
            trusted.score,
            trusted.left_spectral,
            trusted.mid_spectral,
            trusted.side_spectral,
            trusted.rms_ratio,
        );
        eprintln!(
            "Trusted window emu/ref: raw={trusted_raw_corr:.4} spectral={trusted_spectral:.4} rms={trusted_rms_ratio:.4} mid={trusted_mid_spectral:.4} side={trusted_side_spectral:.4}"
        );
    }
    if let Some((target, score, ref_count)) = mono_consensus_oracle {
        eprintln!(
            "Mono consensus oracle: refs={ref_count} self_spectral={:.4} self_rms_fit={:.4} average={:.4} penalty={:.4} final={:.4}",
            target.self_spectral,
            target.self_rms_fit,
            score.average_score,
            score.disagreement_penalty,
            score.final_score
        );
    }
    if let Some((score, ref_count)) = hybrid_mono_consensus_oracle {
        eprintln!(
            "Hybrid mono consensus: refs={ref_count} average={:.4} penalty={:.4} final={:.4}",
            score.average_score, score.disagreement_penalty, score.final_score
        );
    }
    if let Some((
        score,
        section_count,
        (dominant_start, dominant_len, dominant_score, dominant_impact),
        (weak_start, weak_len, weak_score),
    )) = sectioned_side_consensus_oracle
    {
        eprintln!(
            "Sectioned side consensus: sections={section_count} average={:.4} penalty={:.4} final={:.4} worst_section={:.4} dominant_impact={:.4}",
            score.average_score,
            score.disagreement_penalty,
            score.final_score,
            score.worst_section_score,
            score.dominant_section_impact,
        );
        eprintln!(
            "Sectioned side dominant: start={:.2}s end={:.2}s final={dominant_score:.4} impact={dominant_impact:.4}",
            dominant_start as f32 / SAMPLE_RATE as f32,
            (dominant_start + dominant_len) as f32 / SAMPLE_RATE as f32,
        );
        eprintln!(
            "Sectioned side raw weakest: start={:.2}s end={:.2}s final={weak_score:.4}",
            weak_start as f32 / SAMPLE_RATE as f32,
            (weak_start + weak_len) as f32 / SAMPLE_RATE as f32,
        );
    }
    if let Some((
        score,
        section_count,
        (dominant_start, dominant_len, dominant_score, dominant_impact),
        (
            weak_start,
            weak_len,
            weak_score,
            weak_lag_bins,
            weak_adjusted_env,
            weak_transient,
            weak_adjusted_transient,
            weak_transient_rms,
        ),
        weak_reference_transient,
    )) = sectioned_side_dynamics_oracle
    {
        eprintln!(
            "Sectioned side dynamics: sections={section_count} average={:.4} penalty={:.4} final={:.4} worst_section={:.4} dominant_impact={:.4}",
            score.average_score,
            score.disagreement_penalty,
            score.final_score,
            score.worst_section_score,
            score.dominant_section_impact,
        );
        eprintln!(
            "Sectioned side dynamics dominant: start={:.2}s end={:.2}s final={dominant_score:.4} impact={dominant_impact:.4}",
            dominant_start as f32 / SAMPLE_RATE as f32,
            (dominant_start + dominant_len) as f32 / SAMPLE_RATE as f32,
        );
        eprintln!(
            "Sectioned side dynamics raw weakest: start={:.2}s end={:.2}s final={weak_score:.4}",
            weak_start as f32 / SAMPLE_RATE as f32,
            (weak_start + weak_len) as f32 / SAMPLE_RATE as f32,
        );
        eprintln!(
            "Sectioned side dynamics weakest lag: bins={weak_lag_bins:+} ms={:+.1} adjusted_env={weak_adjusted_env:.4}",
            weak_lag_bins as f32 * SIDE_DYNAMICS_ENV_WINDOW as f32 * 1000.0 / SAMPLE_RATE as f32,
        );
        eprintln!(
            "Sectioned side dynamics weakest transient: raw={weak_transient:.4} adjusted={weak_adjusted_transient:.4} rms={weak_transient_rms:.4}"
        );
        if let Some(summary) = weak_reference_transient {
            eprintln!(
                "Sectioned side dynamics weakest reference transient: pairs={} env={:.4}->{:.4} trans={:.4}->{:.4} rms_fit={:.4} lag={:.1}ms",
                summary.pair_count,
                summary.average_envelope_corr,
                summary.average_adjusted_envelope_corr,
                summary.average_transient_corr,
                summary.average_adjusted_transient_corr,
                summary.average_transient_rms_fit,
                summary.average_abs_lag_bins * SIDE_DYNAMICS_ENV_WINDOW as f32 * 1000.0
                    / SAMPLE_RATE as f32,
            );
        }
    }

    // Minimum bar: emulator produces non-trivial audio
    assert!(
        rms_e > 0.01,
        "Emulator audio RMS too low ({rms_e:.4}), likely silent"
    );

    // If we have a real reference, check correlation
    // Note: correlation with a hardware recording won't be super high due to
    // timing differences, but should be positive (same musical content).
    if ref_rate == SAMPLE_RATE {
        eprintln!("Same sample rate — direct comparison valid");
        // A correlation > 0.3 means the musical content is detectably similar
        // > 0.7 would mean quite accurate
        if corr > 0.5 {
            eprintln!("GOOD: Strong correlation with reference");
        } else if corr > 0.2 {
            eprintln!("OK: Moderate correlation — audible similarity");
        } else {
            eprintln!("WEAK: Low correlation — may need further accuracy work");
        }

        assert!(
            local_env_corr > 0.6,
            "Expected a strong local GHZ window match, got local envelope correlation {local_env_corr:.4} (global env {env_corr:.4}, best lag {env_lag_secs:.2}s, raw corr {corr:.4})"
        );
        assert!(
            local_spectral > 0.6,
            "Expected recognizable local spectral similarity, got {local_spectral:.4} (env {local_env_corr:.4}, raw {local_raw_corr:.4})"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_output_profiles() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let ref_dir = Path::new(REFERENCE_DIR);
    let ref_path = ["sonic_ghz.flac", "sonic_ghz.wav"]
        .iter()
        .map(|f| ref_dir.join(f))
        .find(|p| p.exists())
        .expect("expected GHZ reference audio");
    let (_ref_rate, ref_samples) = load_reference(&ref_path).expect("failed to load reference");

    let record_frames = ((ref_samples.len() as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);

    let ref_left: Vec<f32> = ref_samples.iter().step_by(2).copied().collect();
    let env_window = 2048usize;
    let ref_env = rms_envelope(&ref_left, env_window);
    let spectral_bins = log_frequency_bins(2048, SAMPLE_RATE, 80.0, 12_000.0, 24);

    let base = AudioOutputConfig::legacy()
        .with_gain(2.5)
        .with_ym_gain(1.1)
        .with_psg_gain(0.65)
        .with_stereo_crossfeed(0.35)
        .with_post_low_pass_hz(12_000.0);
    let current_default = AudioOutputConfig::default();
    let fir_fit = {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(current_default);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        let emu_left: Vec<f32> = rendered.iter().step_by(2).copied().collect();
        let emu_env = rms_envelope(&emu_left, env_window);
        let max_lag_windows = emu_env.len().min(ref_env.len()).saturating_sub(64);
        let (_env_corr, env_lag) = best_lagged_correlation(&emu_env, &ref_env, max_lag_windows);
        let local_env_window = (SAMPLE_RATE as usize * 8 / env_window).max(64);
        let local_match =
            best_windowed_match_at_lag(&emu_env, &ref_env, env_lag, local_env_window).unwrap();
        let local_emu_start = local_match.a_start * env_window;
        let local_ref_start = local_match.b_start * env_window;
        let local_raw_len = (local_match.len * env_window)
            .min(emu_left.len().saturating_sub(local_emu_start))
            .min(ref_left.len().saturating_sub(local_ref_start));
        fit_fir_taps_least_squares(
            &emu_left[local_emu_start..local_emu_start + local_raw_len],
            &ref_left[local_ref_start..local_ref_start + local_raw_len],
            1e-3,
        )
    };
    eprintln!("fitted GHZ FIR taps: {:?}", fir_fit);
    let lp12 = base.with_post_low_pass_hz(12_000.0);
    let mut candidates = vec![
        ("current_default".to_owned(), current_default),
        (
            "model1_va2_2.5".to_owned(),
            AudioOutputConfig::new(AudioOutputProfile::Model1Va2, 2.5),
        ),
        ("default_lp12000".to_owned(), lp12),
        (
            "capture_eq_mild".to_owned(),
            with_capture_eq(base, 110.0, -6.0, 380.0, 0.65, 4.5, 2_600.0, -2.5),
        ),
        (
            "capture_eq_base".to_owned(),
            with_capture_eq(base, 110.0, -8.0, 420.0, 0.75, 6.0, 2_600.0, -3.5),
        ),
        (
            "capture_eq_wide".to_owned(),
            with_capture_eq(base, 100.0, -7.0, 360.0, 0.55, 5.5, 3_000.0, -3.0),
        ),
        (
            "capture_eq_strong".to_owned(),
            with_capture_eq(base, 120.0, -8.0, 450.0, 0.90, 7.0, 2_200.0, -4.0),
        ),
        (
            "capture_eq_lowmid_full".to_owned(),
            with_capture_eq_extended(
                base, 110.0, -6.5, 380.0, 0.65, 4.5, 2_600.0, -2.8, 190.0, 0.95, 3.5, 760.0, 1.0,
                1.8,
            ),
        ),
        (
            "capture_eq_body_notch".to_owned(),
            with_capture_eq_extended(
                base, 110.0, -6.0, 380.0, 0.65, 4.5, 2_600.0, -2.8, 190.0, 0.90, 3.2, 560.0, 1.20,
                -2.4,
            ),
        ),
        (
            "capture_eq_precision".to_owned(),
            with_capture_eq_extended(
                base, 115.0, -6.8, 400.0, 0.70, 4.8, 2_800.0, -3.1, 185.0, 1.00, 3.8, 800.0, 1.10,
                1.4,
            ),
        ),
        (
            "side_eq_presence".to_owned(),
            current_default
                .with_side_gain(1.02)
                .with_post_side_eq_1(AudioEqStage::peaking(420.0, 0.95, -1.6))
                .with_post_side_eq_2(AudioEqStage::peaking(900.0, 1.00, 1.3)),
        ),
        (
            "side_eq_air".to_owned(),
            current_default
                .with_side_gain(1.00)
                .with_post_side_eq_1(AudioEqStage::peaking(420.0, 0.95, -1.6))
                .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 0.8)),
        ),
        (
            "side_eq_trim".to_owned(),
            current_default
                .with_side_gain(0.98)
                .with_post_side_eq_1(AudioEqStage::peaking(400.0, 0.85, -1.8))
                .with_post_side_eq_2(AudioEqStage::peaking(2_100.0, 0.90, -1.0)),
        ),
        (
            "side_eq_combo".to_owned(),
            current_default
                .with_side_gain(1.01)
                .with_post_side_eq_1(AudioEqStage::peaking(410.0, 0.90, -1.7))
                .with_post_side_eq_2(AudioEqStage::peaking(880.0, 1.05, 1.2)),
        ),
        (
            "capture_eq_peak_only".to_owned(),
            base.with_post_eq_2(AudioEqStage::peaking(420.0, 0.75, 6.0)),
        ),
        (
            "capture_eq_shelves_only".to_owned(),
            base.with_post_eq_1(AudioEqStage::low_shelf(110.0, -8.0))
                .with_post_eq_3(AudioEqStage::high_shelf(2_600.0, -3.5)),
        ),
        (
            "capture_fir_fit_half".to_owned(),
            current_default.with_post_fir_taps(mix_fir_taps(
                [1.0, 0.0, 0.0, 0.0, 0.0],
                fir_fit,
                0.5,
            )),
        ),
        (
            "capture_fir_fit_full".to_owned(),
            current_default.with_post_fir_taps(fir_fit),
        ),
        (
            "capture_fir_delay".to_owned(),
            current_default.with_post_fir_taps([0.92, 0.08, 0.0, 0.0, 0.0]),
        ),
        (
            "capture_fir_soft".to_owned(),
            current_default.with_post_fir_taps([0.84, 0.12, 0.04, 0.0, 0.0]),
        ),
        (
            "capture_fir_symmetric".to_owned(),
            current_default.with_post_fir_taps([0.12, 0.76, 0.12, 0.0, 0.0]),
        ),
        (
            "current_default_ldelay1".to_owned(),
            current_default.with_post_left_delay_samples(1),
        ),
        (
            "current_default_rdelay1".to_owned(),
            current_default.with_post_right_delay_samples(1),
        ),
        (
            "current_default_ldelay2".to_owned(),
            current_default.with_post_left_delay_samples(2),
        ),
        (
            "current_default_rdelay2".to_owned(),
            current_default.with_post_right_delay_samples(2),
        ),
        (
            "current_default_ldelay1_xf40".to_owned(),
            current_default
                .with_post_left_delay_samples(1)
                .with_stereo_crossfeed(0.40),
        ),
        (
            "current_default_rdelay1_xf40".to_owned(),
            current_default
                .with_post_right_delay_samples(1)
                .with_stereo_crossfeed(0.40),
        ),
    ];
    for crossfeed in [0.05f32, 0.10, 0.15, 0.20, 0.25, 0.30, 0.35] {
        candidates.push((
            format!("default_xf{:.0}", crossfeed * 100.0),
            base.with_stereo_crossfeed(crossfeed),
        ));
        candidates.push((
            format!("default_lp12000_xf{:.0}", crossfeed * 100.0),
            lp12.with_stereo_crossfeed(crossfeed),
        ));
    }

    let mut scored = Vec::new();
    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let capture_start = (trace.capture_start_sample as usize)
            .saturating_mul(2)
            .min(rendered_full.len());
        let rendered = &rendered_full[capture_start..];
        let emu_left: Vec<f32> = rendered.iter().step_by(2).copied().collect();

        let emu_env = rms_envelope(&emu_left, env_window);
        let max_lag_windows = emu_env.len().min(ref_env.len()).saturating_sub(64);
        let (env_corr, env_lag) = best_lagged_correlation(&emu_env, &ref_env, max_lag_windows);
        let local_env_window = (SAMPLE_RATE as usize * 8 / env_window).max(64);
        let local_match =
            best_windowed_match_at_lag(&emu_env, &ref_env, env_lag, local_env_window).unwrap();
        let local_emu_start = local_match.a_start * env_window;
        let local_ref_start = local_match.b_start * env_window;
        let local_raw_len = (local_match.len * env_window)
            .min(emu_left.len().saturating_sub(local_emu_start))
            .min(ref_left.len().saturating_sub(local_ref_start));
        let spectral = spectral_similarity(
            &emu_left[local_emu_start..local_emu_start + local_raw_len],
            &ref_left[local_ref_start..local_ref_start + local_raw_len],
            2048,
            1024,
            &spectral_bins,
        );
        let rms_ratio = rms(&emu_left[local_emu_start..local_emu_start + local_raw_len])
            / rms(&ref_left[local_ref_start..local_ref_start + local_raw_len]).max(1e-9);
        let rms_fit = 1.0 / (1.0 + (rms_ratio - 1.0).abs());
        let emu_stereo_local = stereo_window(&rendered, local_emu_start, local_raw_len);
        let ref_stereo_local = stereo_window(&ref_samples, local_ref_start, local_raw_len);
        let (emu_mid, emu_side) = stereo_mid_side(emu_stereo_local);
        let (ref_mid, ref_side) = stereo_mid_side(ref_stereo_local);
        let emu_lr_corr = stereo_lr_correlation(emu_stereo_local);
        let ref_lr_corr = stereo_lr_correlation(ref_stereo_local);
        let emu_side_ratio = stereo_side_ratio(emu_stereo_local);
        let ref_side_ratio = stereo_side_ratio(ref_stereo_local);
        let mid_spectral = spectral_similarity(&emu_mid, &ref_mid, 2048, 1024, &spectral_bins);
        let side_spectral = spectral_similarity(&emu_side, &ref_side, 2048, 1024, &spectral_bins);
        let (_emu_lr_lag_corr, emu_lr_lag) = stereo_interchannel_lag(emu_stereo_local, 4);
        let (_ref_lr_lag_corr, ref_lr_lag) = stereo_interchannel_lag(ref_stereo_local, 4);
        let lr_fit = 1.0 / (1.0 + (emu_lr_corr - ref_lr_corr).abs() * 4.0);
        let side_fit = 1.0 / (1.0 + (emu_side_ratio - ref_side_ratio).abs() * 4.0);
        let lag_fit = 1.0 / (1.0 + (emu_lr_lag - ref_lr_lag).abs() as f32 * 0.25);
        let stereo_fit = (lr_fit + side_fit + lag_fit) / 3.0;
        let score = (local_match.corr + spectral + rms_fit + stereo_fit) / 4.0;
        scored.push((
            name,
            config.profile,
            config.master_gain,
            config.ym_gain,
            config.psg_gain,
            config.stereo_crossfeed,
            config.post_high_pass_hz,
            config.post_low_pass_hz,
            env_corr,
            local_match.corr,
            spectral,
            rms_ratio,
            emu_lr_corr,
            ref_lr_corr,
            emu_side_ratio,
            ref_side_ratio,
            mid_spectral,
            side_spectral,
            emu_lr_lag,
            ref_lr_lag,
            stereo_fit,
            score,
        ));
    }

    scored.sort_by(|a, b| {
        b.21.partial_cmp(&a.21)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.10.partial_cmp(&a.10).unwrap_or(std::cmp::Ordering::Equal))
    });
    eprintln!(
        "=== GHZ output profile sweep === extra_frames={} capture_start_tick={} capture_start_sample={} ym_writes={} psg_writes={}",
        trace.extra_frames,
        trace.capture_start_tick,
        trace.capture_start_sample,
        trace.ym_writes.len(),
        trace.psg_writes.len()
    );
    for (
        name,
        profile,
        gain,
        ym_gain,
        psg_gain,
        stereo_crossfeed,
        post_high_pass_hz,
        post_low_pass_hz,
        env_corr,
        local_env,
        spectral,
        rms_ratio,
        emu_lr_corr,
        ref_lr_corr,
        emu_side_ratio,
        ref_side_ratio,
        mid_spectral,
        side_spectral,
        emu_lr_lag,
        ref_lr_lag,
        stereo_fit,
        score,
    ) in scored
    {
        eprintln!(
            "{name:>28}: profile={profile:?} gain={gain:.2} ym={ym_gain:.2} psg={psg_gain:.2} xf={stereo_crossfeed:.2} hp={post_high_pass_hz:?} lp={post_low_pass_hz:?} env={env_corr:.4} local_env={local_env:.4} spectral={spectral:.4} rms_ratio={rms_ratio:.4} lr=({emu_lr_corr:.4}/{ref_lr_corr:.4}) side=({emu_side_ratio:.4}/{ref_side_ratio:.4}) ms=({mid_spectral:.4}/{side_spectral:.4}) lag=({emu_lr_lag:+}/{ref_lr_lag:+}) stereo_fit={stereo_fit:.4} score={score:.4}"
        );
    }
}

/// Self-test: generate a WAV dump of the emulator's audio for manual inspection.
/// This writes to the reference_audio dir for easy A/B comparison.
#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn generate_sonic_ghz_wav() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let ref_dir = Path::new(REFERENCE_DIR);
    std::fs::create_dir_all(ref_dir).ok();

    // Actually enter GHZ, then record ~10s of level music/audio.
    let emu_samples = generate_ghz_audio(&rom, 600);

    let out_path = ref_dir.join("sonic_ghz_emu.wav");
    write_wav(&out_path, SAMPLE_RATE, &emu_samples);

    let duration = emu_samples.len() as f64 / (2.0 * SAMPLE_RATE as f64);
    eprintln!(
        "Wrote {} ({:.1}s, {} samples)",
        out_path.display(),
        duration,
        emu_samples.len()
    );
}

/// Generate a WAV of the title screen music (FM-active period).
/// Skips to frame 480 (after SEGA jingle finishes) and records 300 frames (~5s).
#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn generate_sonic_title_wav() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let ref_dir = Path::new(REFERENCE_DIR);
    std::fs::create_dir_all(ref_dir).ok();

    // Skip 480 frames (past SEGA jingle), record 300 frames of title music
    let emu_samples = generate_emu_audio(&rom, 480, 300);

    // Analyze the audio quality
    let left: Vec<f32> = emu_samples.iter().step_by(2).copied().collect();
    let right: Vec<f32> = emu_samples.iter().skip(1).step_by(2).copied().collect();

    let peak_l = left.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
    let peak_r = right.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
    let rms_l = rms(&left);
    let rms_r = rms(&right);

    // DC offset
    let dc_l: f32 = left.iter().sum::<f32>() / left.len() as f32;
    let dc_r: f32 = right.iter().sum::<f32>() / right.len() as f32;

    // Zero crossing rate (indicator of frequency content)
    let zcr_l = left
        .windows(2)
        .filter(|w| w[0].signum() != w[1].signum())
        .count();
    let zcr_freq = zcr_l as f64 / (2.0 * left.len() as f64 / SAMPLE_RATE as f64);

    // Clipping count (samples at ±1.0)
    let clip_count = emu_samples.iter().filter(|&&s| s.abs() >= 0.999).count();

    eprintln!("=== Title Music Audio Quality ===");
    eprintln!(
        "Samples: {} ({:.2}s)",
        emu_samples.len(),
        emu_samples.len() as f64 / (2.0 * SAMPLE_RATE as f64)
    );
    eprintln!("Peak:  L={peak_l:.4}  R={peak_r:.4}");
    eprintln!("RMS:   L={rms_l:.4}  R={rms_r:.4}");
    eprintln!("DC:    L={dc_l:.6}  R={dc_r:.6}");
    eprintln!("ZCR freq: {zcr_freq:.1} Hz");
    eprintln!("Clipping: {clip_count} samples");

    // Write WAV
    let out_path = ref_dir.join("sonic_title_emu.wav");
    write_wav(&out_path, SAMPLE_RATE, &emu_samples);
    eprintln!("Wrote {}", out_path.display());

    // Basic quality assertions
    assert!(rms_l > 0.01, "Left channel too quiet: {rms_l}");
    assert!(rms_r > 0.01, "Right channel too quiet: {rms_r}");
    assert!(dc_l.abs() < 0.05, "Left DC offset too large: {dc_l}");
    assert!(dc_r.abs() < 0.05, "Right DC offset too large: {dc_r}");
    assert!(clip_count < 100, "Too much clipping: {clip_count} samples");
}

/// Distortion characterization: analyzes the Sonic title audio for specific
/// distortion signatures (clipping, aliasing, discontinuities).
#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn distortion_analysis() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let emu_samples = generate_emu_audio(&rom, 480, 300);
    let left: Vec<f32> = emu_samples.iter().step_by(2).copied().collect();
    let n = left.len();

    eprintln!(
        "=== DISTORTION ANALYSIS ({n} samples, {:.2}s) ===",
        n as f64 / SAMPLE_RATE as f64
    );

    // 1. Amplitude histogram (detect clipping)
    let mut hist = [0u32; 20]; // 0.05 bins from 0.0 to 1.0
    for &s in &left {
        let bin = ((s.abs() * 20.0).min(19.0)) as usize;
        hist[bin] += 1;
    }
    eprintln!("\n--- Amplitude histogram ---");
    for (i, &count) in hist.iter().enumerate() {
        let lo = i as f32 * 0.05;
        let hi = lo + 0.05;
        let pct = count as f64 / n as f64 * 100.0;
        let bar: String = std::iter::repeat('#').take((pct * 2.0) as usize).collect();
        eprintln!("  [{lo:.2}-{hi:.2}]: {count:6} ({pct:5.1}%) {bar}");
    }

    // 2. Sample-to-sample delta distribution (detect discontinuities/clicks)
    let deltas: Vec<f32> = left.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
    let max_delta = deltas.iter().copied().fold(0.0f32, f32::max);
    let mean_delta: f32 = deltas.iter().sum::<f32>() / deltas.len() as f32;
    let large_jumps = deltas.iter().filter(|&&d| d > 0.1).count();
    let huge_jumps = deltas.iter().filter(|&&d| d > 0.2).count();

    eprintln!("\n--- Sample-to-sample deltas ---");
    eprintln!("  Mean delta:   {mean_delta:.6}");
    eprintln!("  Max delta:    {max_delta:.6}");
    eprintln!("  Jumps > 0.1:  {large_jumps}");
    eprintln!("  Jumps > 0.2:  {huge_jumps}");

    // 3. High-frequency energy ratio (detect aliasing)
    // Use a simple difference filter as a high-pass proxy
    let hp: Vec<f32> = left.windows(2).map(|w| w[1] - w[0]).collect();
    let hp_energy: f64 = hp.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let total_energy: f64 = left.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let hf_ratio = hp_energy / total_energy;
    eprintln!("\n--- High-frequency energy ---");
    eprintln!("  HF energy / total: {hf_ratio:.4}");
    eprintln!("  (>0.5 suggests excessive HF content / aliasing)");

    // 4. Check for repeated identical samples (quantization/stuck output)
    let mut repeated = 0u32;
    for w in left.windows(2) {
        if w[0] == w[1] {
            repeated += 1;
        }
    }
    let repeated_pct = repeated as f64 / n as f64 * 100.0;
    eprintln!("\n--- Quantization ---");
    eprintln!("  Repeated consecutive samples: {repeated} ({repeated_pct:.1}%)");

    // 5. Unique amplitude levels (reveals DAC quantization)
    let mut unique: std::collections::HashSet<i32> = std::collections::HashSet::new();
    for &s in &left {
        unique.insert((s * 100000.0) as i32); // ~0.00001 resolution
    }
    eprintln!("  Unique amplitude levels: {}", unique.len());

    // 6. Frame boundary discontinuity check
    // At 44100 Hz / 59.92 fps ≈ 736 samples per frame
    let frame_size = (SAMPLE_RATE as f64 / 59.92) as usize;
    let mut frame_boundary_jumps = Vec::new();
    for i in (frame_size..n).step_by(frame_size) {
        if i < n {
            let delta = (left[i] - left[i - 1]).abs();
            if delta > mean_delta * 5.0 {
                frame_boundary_jumps.push((i, delta));
            }
        }
    }
    eprintln!("\n--- Frame boundary analysis ---");
    eprintln!("  Frame size: ~{frame_size} samples");
    eprintln!(
        "  Suspiciously large boundary jumps (>5x mean): {}",
        frame_boundary_jumps.len()
    );
    for &(idx, delta) in frame_boundary_jumps.iter().take(5) {
        eprintln!("    sample {idx}: delta={delta:.6}");
    }

    // 7. Per-channel clipping estimate
    // Count how many samples are near the theoretical per-channel max
    // Each channel output is ±256/1536 ≈ ±0.167 of the full scale
    let channel_max = 256.0 / 1536.0;
    let near_channel_clip = left
        .iter()
        .filter(|&&s| s.abs() > channel_max * 5.5)
        .count();
    eprintln!("\n--- Level analysis ---");
    eprintln!("  Single channel max: {channel_max:.4}");
    eprintln!(
        "  Samples > 5.5 channels worth ({:.4}): {near_channel_clip}",
        channel_max * 5.5
    );
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_title_distortion_sign_align_candidates() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let candidates = [
        ("current_default", AudioOutputConfig::default()),
        (
            "sign_align_ch4_0p25",
            AudioOutputConfig::default()
                .with_ym_channel_side_sign_align_mixes([0.0, 0.0, 0.0, 0.25, 0.0, 0.0]),
        ),
        (
            "sign_align_ch4_0p50",
            AudioOutputConfig::default()
                .with_ym_channel_side_sign_align_mixes([0.0, 0.0, 0.0, 0.50, 0.0, 0.0]),
        ),
    ];

    eprintln!("=== TITLE DISTORTION SIGN-ALIGN CANDIDATES ===");
    for (name, config) in candidates {
        let emu_samples = generate_emu_audio_with_config(&rom, 480, 300, config);
        let left: Vec<f32> = emu_samples.iter().step_by(2).copied().collect();
        let n = left.len().max(1);
        let peak = left.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        let rms = rms(&left);
        let deltas: Vec<f32> = left.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
        let mean_delta = if deltas.is_empty() {
            0.0
        } else {
            deltas.iter().sum::<f32>() / deltas.len() as f32
        };
        let max_delta = deltas.iter().copied().fold(0.0f32, f32::max);
        let hp: Vec<f32> = left.windows(2).map(|w| w[1] - w[0]).collect();
        let hp_energy: f64 = hp.iter().map(|&s| (s as f64) * (s as f64)).sum();
        let total_energy: f64 = left.iter().map(|&s| (s as f64) * (s as f64)).sum();
        let hf_ratio = if total_energy > 0.0 {
            hp_energy / total_energy
        } else {
            0.0
        };
        let repeated = left.windows(2).filter(|w| w[0] == w[1]).count();
        let repeated_pct = repeated as f64 / n as f64 * 100.0;
        let clipped = left.iter().filter(|&&s| s.abs() >= 0.999).count();

        eprintln!(
            "{name:>20}: peak={peak:.4} rms={rms:.4} hf={hf_ratio:.4} mean_delta={mean_delta:.6} max_delta={max_delta:.6} repeated={repeated_pct:.2}% clipped={clipped}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_distortion_sign_align_candidates() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let candidates = [
        ("current_default", AudioOutputConfig::default()),
        (
            "sign_align_ch4_0p25",
            AudioOutputConfig::default()
                .with_ym_channel_side_sign_align_mixes([0.0, 0.0, 0.0, 0.25, 0.0, 0.0]),
        ),
        (
            "sign_align_ch4_0p50",
            AudioOutputConfig::default()
                .with_ym_channel_side_sign_align_mixes([0.0, 0.0, 0.0, 0.50, 0.0, 0.0]),
        ),
    ];

    eprintln!("=== GHZ DISTORTION SIGN-ALIGN CANDIDATES ===");
    for (name, config) in candidates {
        let emu_samples = generate_ghz_audio_with_config(&rom, 300, config);
        let left: Vec<f32> = emu_samples.iter().step_by(2).copied().collect();
        let n = left.len().max(1);
        let peak = left.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        let rms = rms(&left);
        let deltas: Vec<f32> = left.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
        let mean_delta = if deltas.is_empty() {
            0.0
        } else {
            deltas.iter().sum::<f32>() / deltas.len() as f32
        };
        let max_delta = deltas.iter().copied().fold(0.0f32, f32::max);
        let hp: Vec<f32> = left.windows(2).map(|w| w[1] - w[0]).collect();
        let hp_energy: f64 = hp.iter().map(|&s| (s as f64) * (s as f64)).sum();
        let total_energy: f64 = left.iter().map(|&s| (s as f64) * (s as f64)).sum();
        let hf_ratio = if total_energy > 0.0 {
            hp_energy / total_energy
        } else {
            0.0
        };
        let repeated = left.windows(2).filter(|w| w[0] == w[1]).count();
        let repeated_pct = repeated as f64 / n as f64 * 100.0;
        let clipped = left.iter().filter(|&&s| s.abs() >= 0.999).count();

        eprintln!(
            "{name:>20}: peak={peak:.4} rms={rms:.4} hf={hf_ratio:.4} mean_delta={mean_delta:.6} max_delta={max_delta:.6} repeated={repeated_pct:.2}% clipped={clipped}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_master_gain_clip_tradeoff() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    eprintln!("=== MASTER GAIN CLIP TRADEOFF ===");
    for gain in [2.20f32, 2.25, 2.30, 2.35, 2.40, 2.45, 2.50] {
        let config = current_default.with_gain(gain);
        let title = generate_emu_audio_with_config(&rom, 480, 300, config);
        let title_clip = title.iter().filter(|&&s| s.abs() >= 0.999).count();
        let ghz = generate_ghz_audio_with_config(&rom, 300, config);
        let ghz_clip = ghz.iter().filter(|&&s| s.abs() >= 0.999).count();

        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];

        let Some(mono) =
            score_fixed_mono_consensus_candidate("candidate", rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(hybrid) = score_fixed_hybrid_mono_consensus_candidate(
            "candidate",
            rendered,
            &mono_refs,
            &loaded_refs,
            &mono_target,
        ) else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate("candidate", rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate("candidate", rendered, &dynamics_targets)
        else {
            continue;
        };

        eprintln!(
            "gain={gain:.2}: title_clip={title_clip} ghz_clip={ghz_clip} mono={:.4} hybrid={:.4} side={:.4} dyn={:.4}",
            mono.final_score, hybrid.final_score, side_cons.final_score, side_dyn.final_score
        );
    }
}

fn write_wav(path: &Path, sample_rate: u32, samples: &[f32]) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).expect("Failed to create WAV");
    for &sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        writer
            .write_sample((clamped * 32767.0) as i16)
            .expect("write failed");
    }
    writer.finalize().expect("finalize failed");
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_side_memory_decay_candidates() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let mut results = Vec::new();
    for &(ch1_ms, ch4_ms, ch5_ms) in &[
        (0.0f32, 0.25f32, 0.25f32),
        (0.0f32, 0.50f32, 0.50f32),
        (0.25f32, 0.50f32, 0.50f32),
        (0.25f32, 1.00f32, 0.50f32),
        (0.25f32, 1.00f32, 1.00f32),
        (0.50f32, 1.00f32, 1.00f32),
        (0.50f32, 2.00f32, 1.00f32),
        (0.50f32, 2.00f32, 2.00f32),
        (1.00f32, 2.00f32, 2.00f32),
        (1.00f32, 4.00f32, 2.00f32),
        (1.00f32, 4.00f32, 4.00f32),
        (2.00f32, 4.00f32, 4.00f32),
        (2.00f32, 8.00f32, 4.00f32),
        (2.00f32, 8.00f32, 8.00f32),
        (4.00f32, 8.00f32, 8.00f32),
    ] {
        let config =
            current_default.with_ym_channel_side_decay_ms([ch1_ms, 0.0, 0.0, ch4_ms, ch5_ms, 0.0]);
        let name = format!("ch1_{ch1_ms:.2}ms_ch4_{ch4_ms:.2}ms_ch5_{ch5_ms:.2}ms");
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];
        let Some(mono) =
            score_fixed_mono_consensus_candidate(&name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(&name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(&name, rendered, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name,
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.push((
        "current_default".to_owned(),
        score_fixed_mono_consensus_candidate(
            "current_default",
            anchor_rendered,
            &mono_refs,
            &mono_target,
        )
        .expect("expected mono score")
        .final_score,
        score_fixed_sectioned_side_consensus_candidate(
            "current_default",
            anchor_rendered,
            &side_targets,
        )
        .expect("expected side score")
        .final_score,
        score_fixed_sectioned_side_dynamics_candidate(
            "current_default",
            anchor_rendered,
            &dynamics_targets,
        )
        .expect("expected dynamics score")
        .final_score,
        0.0,
    ));
    if let Some(last) = results.last_mut() {
        last.4 = last.1 * 0.70 + last.2 * 0.20 + last.3 * 0.10;
    }

    results.sort_by(|a, b| {
        b.3.partial_cmp(&a.3)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ side-memory decay candidates ===");
    for (name, mono_final, side_final, dyn_final, combined) in results {
        eprintln!(
            "{name}: mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_side_memory_micro_decay_candidates() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let mut results = Vec::new();
    for &(ch1_ms, ch4_ms, ch5_ms) in &[
        (0.01f32, 0.01f32, 0.01f32),
        (0.01f32, 0.02f32, 0.02f32),
        (0.02f32, 0.02f32, 0.02f32),
        (0.02f32, 0.04f32, 0.02f32),
        (0.02f32, 0.04f32, 0.04f32),
        (0.04f32, 0.04f32, 0.04f32),
        (0.04f32, 0.06f32, 0.04f32),
        (0.04f32, 0.06f32, 0.06f32),
        (0.06f32, 0.06f32, 0.06f32),
        (0.06f32, 0.08f32, 0.06f32),
        (0.06f32, 0.08f32, 0.08f32),
        (0.08f32, 0.08f32, 0.08f32),
        (0.08f32, 0.12f32, 0.08f32),
        (0.08f32, 0.12f32, 0.12f32),
        (0.12f32, 0.12f32, 0.12f32),
    ] {
        let config =
            current_default.with_ym_channel_side_decay_ms([ch1_ms, 0.0, 0.0, ch4_ms, ch5_ms, 0.0]);
        let name = format!("ch1_{ch1_ms:.2}ms_ch4_{ch4_ms:.2}ms_ch5_{ch5_ms:.2}ms");
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];
        let Some(mono) =
            score_fixed_mono_consensus_candidate(&name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(&name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(&name, rendered, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name,
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.push((
        "current_default".to_owned(),
        score_fixed_mono_consensus_candidate(
            "current_default",
            anchor_rendered,
            &mono_refs,
            &mono_target,
        )
        .expect("expected mono score")
        .final_score,
        score_fixed_sectioned_side_consensus_candidate(
            "current_default",
            anchor_rendered,
            &side_targets,
        )
        .expect("expected side score")
        .final_score,
        score_fixed_sectioned_side_dynamics_candidate(
            "current_default",
            anchor_rendered,
            &dynamics_targets,
        )
        .expect("expected dynamics score")
        .final_score,
        0.0,
    ));
    if let Some(last) = results.last_mut() {
        last.4 = last.1 * 0.70 + last.2 * 0.20 + last.3 * 0.10;
    }

    results.sort_by(|a, b| {
        b.3.partial_cmp(&a.3)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ side-memory micro-decay candidates ===");
    for (name, mono_final, side_final, dyn_final, combined) in results {
        eprintln!(
            "{name}: mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_side_memory_micro_decay_amount_refine() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let mut results = Vec::new();
    for &(ch1_amt, ch4_amt, ch5_amt, ch1_ms, ch4_ms, ch5_ms) in &[
        (0.20f32, 0.25f32, 0.15f32, 0.01f32, 0.01f32, 0.01f32),
        (0.20f32, 0.25f32, 0.15f32, 0.01f32, 0.02f32, 0.02f32),
        (0.20f32, 0.20f32, 0.10f32, 0.01f32, 0.01f32, 0.01f32),
        (0.20f32, 0.20f32, 0.10f32, 0.01f32, 0.02f32, 0.02f32),
        (0.20f32, 0.20f32, 0.15f32, 0.01f32, 0.01f32, 0.01f32),
        (0.20f32, 0.20f32, 0.15f32, 0.01f32, 0.02f32, 0.02f32),
        (0.20f32, 0.25f32, 0.10f32, 0.01f32, 0.01f32, 0.01f32),
        (0.20f32, 0.25f32, 0.10f32, 0.01f32, 0.02f32, 0.02f32),
        (0.20f32, 0.30f32, 0.10f32, 0.01f32, 0.01f32, 0.01f32),
        (0.20f32, 0.30f32, 0.10f32, 0.01f32, 0.02f32, 0.02f32),
        (0.20f32, 0.30f32, 0.15f32, 0.01f32, 0.01f32, 0.01f32),
        (0.20f32, 0.30f32, 0.15f32, 0.01f32, 0.02f32, 0.02f32),
    ] {
        let config = current_default
            .with_ym_channel_side_memory_amounts([ch1_amt, 0.0, 0.0, ch4_amt, ch5_amt, 0.0])
            .with_ym_channel_side_decay_ms([ch1_ms, 0.0, 0.0, ch4_ms, ch5_ms, 0.0]);
        let name = format!(
            "a_{ch1_amt:.2}_{ch4_amt:.2}_{ch5_amt:.2}_d_{ch1_ms:.2}_{ch4_ms:.2}_{ch5_ms:.2}"
        );
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];
        let Some(mono) =
            score_fixed_mono_consensus_candidate(&name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(&name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(&name, rendered, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name,
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.push((
        "current_default".to_owned(),
        score_fixed_mono_consensus_candidate(
            "current_default",
            anchor_rendered,
            &mono_refs,
            &mono_target,
        )
        .expect("expected mono score")
        .final_score,
        score_fixed_sectioned_side_consensus_candidate(
            "current_default",
            anchor_rendered,
            &side_targets,
        )
        .expect("expected side score")
        .final_score,
        score_fixed_sectioned_side_dynamics_candidate(
            "current_default",
            anchor_rendered,
            &dynamics_targets,
        )
        .expect("expected dynamics score")
        .final_score,
        0.0,
    ));
    if let Some(last) = results.last_mut() {
        last.4 = last.1 * 0.70 + last.2 * 0.20 + last.3 * 0.10;
    }

    results.sort_by(|a, b| {
        b.3.partial_cmp(&a.3)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ side-memory micro-decay amount refine ===");
    for (name, mono_final, side_final, dyn_final, combined) in results {
        eprintln!(
            "{name}: mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_side_memory_micro_decay_amount_refine_2() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let mut results = Vec::new();
    for &(ch4_amt, ch5_amt, ch4_ms, ch5_ms) in &[
        (0.30f32, 0.10f32, 0.00f32, 0.00f32),
        (0.30f32, 0.10f32, 0.01f32, 0.01f32),
        (0.30f32, 0.10f32, 0.02f32, 0.02f32),
        (0.30f32, 0.10f32, 0.03f32, 0.03f32),
        (0.30f32, 0.15f32, 0.00f32, 0.00f32),
        (0.30f32, 0.15f32, 0.01f32, 0.01f32),
        (0.30f32, 0.15f32, 0.02f32, 0.02f32),
        (0.30f32, 0.15f32, 0.03f32, 0.03f32),
        (0.35f32, 0.10f32, 0.00f32, 0.00f32),
        (0.35f32, 0.10f32, 0.01f32, 0.01f32),
        (0.35f32, 0.10f32, 0.02f32, 0.02f32),
        (0.35f32, 0.10f32, 0.03f32, 0.03f32),
        (0.35f32, 0.15f32, 0.00f32, 0.00f32),
        (0.35f32, 0.15f32, 0.01f32, 0.01f32),
        (0.35f32, 0.15f32, 0.02f32, 0.02f32),
        (0.35f32, 0.15f32, 0.03f32, 0.03f32),
        (0.35f32, 0.20f32, 0.00f32, 0.00f32),
        (0.35f32, 0.20f32, 0.01f32, 0.01f32),
        (0.35f32, 0.20f32, 0.02f32, 0.02f32),
        (0.35f32, 0.20f32, 0.03f32, 0.03f32),
        (0.40f32, 0.10f32, 0.00f32, 0.00f32),
        (0.40f32, 0.10f32, 0.01f32, 0.01f32),
        (0.40f32, 0.10f32, 0.02f32, 0.02f32),
        (0.40f32, 0.10f32, 0.03f32, 0.03f32),
        (0.40f32, 0.15f32, 0.00f32, 0.00f32),
        (0.40f32, 0.15f32, 0.01f32, 0.01f32),
        (0.40f32, 0.15f32, 0.02f32, 0.02f32),
        (0.40f32, 0.15f32, 0.03f32, 0.03f32),
        (0.40f32, 0.20f32, 0.00f32, 0.00f32),
        (0.40f32, 0.20f32, 0.01f32, 0.01f32),
        (0.40f32, 0.20f32, 0.02f32, 0.02f32),
        (0.40f32, 0.20f32, 0.03f32, 0.03f32),
    ] {
        let config = current_default
            .with_ym_channel_side_memory_amounts([0.20, 0.0, 0.0, ch4_amt, ch5_amt, 0.0])
            .with_ym_channel_side_decay_ms([0.01, 0.0, 0.0, ch4_ms, ch5_ms, 0.0]);
        let name = format!("a_0.20_{ch4_amt:.2}_{ch5_amt:.2}_d_0.01_{ch4_ms:.2}_{ch5_ms:.2}");
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];
        let Some(mono) =
            score_fixed_mono_consensus_candidate(&name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(&name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(&name, rendered, &dynamics_targets)
        else {
            continue;
        };
        let combined =
            mono.final_score * 0.70 + side_cons.final_score * 0.20 + side_dyn.final_score * 0.10;
        results.push((
            name,
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
            combined,
        ));
    }

    results.push((
        "current_default".to_owned(),
        score_fixed_mono_consensus_candidate(
            "current_default",
            anchor_rendered,
            &mono_refs,
            &mono_target,
        )
        .expect("expected mono score")
        .final_score,
        score_fixed_sectioned_side_consensus_candidate(
            "current_default",
            anchor_rendered,
            &side_targets,
        )
        .expect("expected side score")
        .final_score,
        score_fixed_sectioned_side_dynamics_candidate(
            "current_default",
            anchor_rendered,
            &dynamics_targets,
        )
        .expect("expected dynamics score")
        .final_score,
        0.0,
    ));
    if let Some(last) = results.last_mut() {
        last.4 = last.1 * 0.70 + last.2 * 0.20 + last.3 * 0.10;
    }

    results.sort_by(|a, b| {
        b.3.partial_cmp(&a.3)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ side-memory micro-decay amount refine 2 ===");
    for (name, mono_final, side_final, dyn_final, combined) in results {
        eprintln!(
            "{name}: mono={mono_final:.4} side={side_final:.4} dyn={dyn_final:.4} combined={combined:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_stereo_candidate_tradeoffs_after_side_memory_promotion() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let candidates = [
        ("current_default", current_default),
        ("xf_0_40", current_default.with_stereo_crossfeed(0.40)),
        (
            "side_q_1_50",
            current_default
                .with_post_side_eq_1(AudioEqStage::peaking(450.0, 1.50, -4.0))
                .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 0.0)),
        ),
        (
            "xf_0_40_plus_side_q_1_50",
            current_default
                .with_stereo_crossfeed(0.40)
                .with_post_side_eq_1(AudioEqStage::peaking(450.0, 1.50, -4.0))
                .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 0.0)),
        ),
    ];

    let mut rows = Vec::new();
    for (name, config) in candidates {
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];

        let Some(mono) =
            score_fixed_mono_consensus_candidate(name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(hybrid) = score_fixed_hybrid_mono_consensus_candidate(
            name,
            rendered,
            &mono_refs,
            &loaded_refs,
            &mono_target,
        ) else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(name, rendered, &dynamics_targets)
        else {
            continue;
        };
        rows.push((
            name,
            mono.final_score,
            hybrid.final_score,
            side_cons.final_score,
            side_dyn.final_score,
        ));
    }

    rows.sort_by(|a, b| {
        b.4.partial_cmp(&a.4)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ stereo candidate tradeoffs after side-memory promotion ===");
    for (name, mono, hybrid, side_cons, side_dyn) in rows {
        eprintln!(
            "{name:>26}: mono={mono:.4} hybrid={hybrid:.4} side={side_cons:.4} dyn={side_dyn:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_side_memory_post_q_refine() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let mut rows = Vec::new();
    for &(ch1, ch4, ch5) in &[
        (0.15f32, 0.40f32, 0.05f32),
        (0.15f32, 0.40f32, 0.10f32),
        (0.15f32, 0.45f32, 0.05f32),
        (0.15f32, 0.45f32, 0.10f32),
        (0.20f32, 0.40f32, 0.05f32),
        (0.20f32, 0.40f32, 0.10f32),
        (0.20f32, 0.45f32, 0.05f32),
        (0.20f32, 0.45f32, 0.10f32),
        (0.20f32, 0.50f32, 0.05f32),
        (0.20f32, 0.50f32, 0.10f32),
        (0.25f32, 0.40f32, 0.05f32),
        (0.25f32, 0.40f32, 0.10f32),
        (0.25f32, 0.45f32, 0.05f32),
        (0.25f32, 0.45f32, 0.10f32),
        (0.25f32, 0.50f32, 0.05f32),
        (0.25f32, 0.50f32, 0.10f32),
    ] {
        let config = current_default
            .with_ym_channel_side_memory_amounts([ch1, 0.0, 0.0, ch4, ch5, 0.0])
            .with_ym_channel_side_decay_ms([0.01, 0.0, 0.0, 0.03, 0.03, 0.0]);
        let name = format!("a_{ch1:.2}_{ch4:.2}_{ch5:.2}");
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];

        let Some(mono) =
            score_fixed_mono_consensus_candidate(&name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(&name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(&name, rendered, &dynamics_targets)
        else {
            continue;
        };

        rows.push((
            name,
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
        ));
    }

    rows.push((
        "current_default".to_owned(),
        score_fixed_mono_consensus_candidate(
            "current_default",
            anchor_rendered,
            &mono_refs,
            &mono_target,
        )
        .expect("expected mono score")
        .final_score,
        score_fixed_sectioned_side_consensus_candidate(
            "current_default",
            anchor_rendered,
            &side_targets,
        )
        .expect("expected side score")
        .final_score,
        score_fixed_sectioned_side_dynamics_candidate(
            "current_default",
            anchor_rendered,
            &dynamics_targets,
        )
        .expect("expected dynamics score")
        .final_score,
    ));

    rows.sort_by(|a, b| {
        b.3.partial_cmp(&a.3)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ side-memory post-Q refine ===");
    for (name, mono, side_cons, side_dyn) in rows {
        eprintln!("{name:>18}: mono={mono:.4} side={side_cons:.4} dyn={side_dyn:.4}");
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_side_memory_post_q_refine_2() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let mut rows = Vec::new();
    for &(ch4, ch5, ch4_ms, ch5_ms) in &[
        (0.50f32, 0.00f32, 0.02f32, 0.02f32),
        (0.50f32, 0.00f32, 0.03f32, 0.03f32),
        (0.50f32, 0.00f32, 0.04f32, 0.04f32),
        (0.50f32, 0.02f32, 0.02f32, 0.02f32),
        (0.50f32, 0.02f32, 0.03f32, 0.03f32),
        (0.50f32, 0.02f32, 0.04f32, 0.04f32),
        (0.50f32, 0.05f32, 0.02f32, 0.02f32),
        (0.50f32, 0.05f32, 0.03f32, 0.03f32),
        (0.50f32, 0.05f32, 0.04f32, 0.04f32),
        (0.55f32, 0.00f32, 0.02f32, 0.02f32),
        (0.55f32, 0.00f32, 0.03f32, 0.03f32),
        (0.55f32, 0.00f32, 0.04f32, 0.04f32),
        (0.55f32, 0.02f32, 0.02f32, 0.02f32),
        (0.55f32, 0.02f32, 0.03f32, 0.03f32),
        (0.55f32, 0.02f32, 0.04f32, 0.04f32),
        (0.55f32, 0.05f32, 0.02f32, 0.02f32),
        (0.55f32, 0.05f32, 0.03f32, 0.03f32),
        (0.55f32, 0.05f32, 0.04f32, 0.04f32),
        (0.60f32, 0.00f32, 0.02f32, 0.02f32),
        (0.60f32, 0.00f32, 0.03f32, 0.03f32),
        (0.60f32, 0.00f32, 0.04f32, 0.04f32),
        (0.60f32, 0.02f32, 0.02f32, 0.02f32),
        (0.60f32, 0.02f32, 0.03f32, 0.03f32),
        (0.60f32, 0.02f32, 0.04f32, 0.04f32),
        (0.60f32, 0.05f32, 0.02f32, 0.02f32),
        (0.60f32, 0.05f32, 0.03f32, 0.03f32),
        (0.60f32, 0.05f32, 0.04f32, 0.04f32),
    ] {
        let config = current_default
            .with_ym_channel_side_memory_amounts([0.20, 0.0, 0.0, ch4, ch5, 0.0])
            .with_ym_channel_side_decay_ms([0.01, 0.0, 0.0, ch4_ms, ch5_ms, 0.0]);
        let name = format!("a_0.20_{ch4:.2}_{ch5:.2}_d_{ch4_ms:.2}_{ch5_ms:.2}");
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];

        let Some(mono) =
            score_fixed_mono_consensus_candidate(&name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(&name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(&name, rendered, &dynamics_targets)
        else {
            continue;
        };

        rows.push((
            name,
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
        ));
    }

    rows.push((
        "current_default".to_owned(),
        score_fixed_mono_consensus_candidate(
            "current_default",
            anchor_rendered,
            &mono_refs,
            &mono_target,
        )
        .expect("expected mono score")
        .final_score,
        score_fixed_sectioned_side_consensus_candidate(
            "current_default",
            anchor_rendered,
            &side_targets,
        )
        .expect("expected side score")
        .final_score,
        score_fixed_sectioned_side_dynamics_candidate(
            "current_default",
            anchor_rendered,
            &dynamics_targets,
        )
        .expect("expected dynamics score")
        .final_score,
    ));

    rows.sort_by(|a, b| {
        b.3.partial_cmp(&a.3)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ side-memory post-Q refine 2 ===");
    for (name, mono, side_cons, side_dyn) in rows {
        eprintln!("{name:>28}: mono={mono:.4} side={side_cons:.4} dyn={side_dyn:.4}");
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_side_memory_post_q_refine_3() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let mut rows = Vec::new();
    for &(ch4, ch5, ch4_ms, ch5_ms) in &[
        (0.60f32, 0.00f32, 0.01f32, 0.01f32),
        (0.60f32, 0.00f32, 0.02f32, 0.02f32),
        (0.60f32, 0.00f32, 0.03f32, 0.03f32),
        (0.60f32, 0.02f32, 0.01f32, 0.01f32),
        (0.60f32, 0.02f32, 0.02f32, 0.02f32),
        (0.60f32, 0.02f32, 0.03f32, 0.03f32),
        (0.65f32, 0.00f32, 0.01f32, 0.01f32),
        (0.65f32, 0.00f32, 0.02f32, 0.02f32),
        (0.65f32, 0.00f32, 0.03f32, 0.03f32),
        (0.65f32, 0.02f32, 0.01f32, 0.01f32),
        (0.65f32, 0.02f32, 0.02f32, 0.02f32),
        (0.65f32, 0.02f32, 0.03f32, 0.03f32),
        (0.70f32, 0.00f32, 0.01f32, 0.01f32),
        (0.70f32, 0.00f32, 0.02f32, 0.02f32),
        (0.70f32, 0.00f32, 0.03f32, 0.03f32),
        (0.70f32, 0.02f32, 0.01f32, 0.01f32),
        (0.70f32, 0.02f32, 0.02f32, 0.02f32),
        (0.70f32, 0.02f32, 0.03f32, 0.03f32),
    ] {
        let config = current_default
            .with_ym_channel_side_memory_amounts([0.20, 0.0, 0.0, ch4, ch5, 0.0])
            .with_ym_channel_side_decay_ms([0.01, 0.0, 0.0, ch4_ms, ch5_ms, 0.0]);
        let name = format!("a_0.20_{ch4:.2}_{ch5:.2}_d_{ch4_ms:.2}_{ch5_ms:.2}");
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];

        let Some(mono) =
            score_fixed_mono_consensus_candidate(&name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(&name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(&name, rendered, &dynamics_targets)
        else {
            continue;
        };

        rows.push((
            name,
            mono.final_score,
            side_cons.final_score,
            side_dyn.final_score,
        ));
    }

    rows.push((
        "current_default".to_owned(),
        score_fixed_mono_consensus_candidate(
            "current_default",
            anchor_rendered,
            &mono_refs,
            &mono_target,
        )
        .expect("expected mono score")
        .final_score,
        score_fixed_sectioned_side_consensus_candidate(
            "current_default",
            anchor_rendered,
            &side_targets,
        )
        .expect("expected side score")
        .final_score,
        score_fixed_sectioned_side_dynamics_candidate(
            "current_default",
            anchor_rendered,
            &dynamics_targets,
        )
        .expect("expected dynamics score")
        .final_score,
    ));

    rows.sort_by(|a, b| {
        b.3.partial_cmp(&a.3)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ side-memory post-Q refine 3 ===");
    for (name, mono, side_cons, side_dyn) in rows {
        eprintln!("{name:>28}: mono={mono:.4} side={side_cons:.4} dyn={side_dyn:.4}");
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_side_memory_transient_mix_refine() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let mut rows = Vec::new();
    for &(ch1_mix, ch4_mix, ch5_mix, ch4_amount, ch5_amount, ch4_ms, ch5_ms) in &[
        (0.0f32, 0.25f32, 0.0f32, 0.60f32, 0.00f32, 0.02f32, 0.02f32),
        (0.0f32, 0.50f32, 0.0f32, 0.60f32, 0.00f32, 0.02f32, 0.02f32),
        (0.0f32, 0.75f32, 0.0f32, 0.60f32, 0.00f32, 0.02f32, 0.02f32),
        (0.0f32, 1.00f32, 0.0f32, 0.60f32, 0.00f32, 0.02f32, 0.02f32),
        (0.20f32, 0.75f32, 0.0f32, 0.60f32, 0.00f32, 0.02f32, 0.02f32),
        (0.20f32, 1.00f32, 0.0f32, 0.60f32, 0.00f32, 0.02f32, 0.02f32),
        (0.0f32, 0.75f32, 0.0f32, 0.65f32, 0.00f32, 0.02f32, 0.02f32),
        (0.0f32, 0.75f32, 0.0f32, 0.70f32, 0.00f32, 0.02f32, 0.02f32),
        (0.0f32, 1.00f32, 0.0f32, 0.65f32, 0.00f32, 0.02f32, 0.02f32),
        (0.0f32, 0.75f32, 0.0f32, 0.60f32, 0.00f32, 0.01f32, 0.01f32),
        (0.0f32, 0.75f32, 0.0f32, 0.60f32, 0.00f32, 0.03f32, 0.03f32),
        (0.0f32, 0.75f32, 0.20f32, 0.60f32, 0.05f32, 0.02f32, 0.02f32),
    ] {
        let config = current_default
            .with_ym_channel_side_memory_amounts([0.20, 0.0, 0.0, ch4_amount, ch5_amount, 0.0])
            .with_ym_channel_side_transient_mixes([ch1_mix, 0.0, 0.0, ch4_mix, ch5_mix, 0.0])
            .with_ym_channel_side_decay_ms([0.01, 0.0, 0.0, ch4_ms, ch5_ms, 0.0]);
        let name = format!(
            "tm_{ch1_mix:.2}_{ch4_mix:.2}_{ch5_mix:.2}_a_{ch4_amount:.2}_{ch5_amount:.2}_d_{ch4_ms:.2}_{ch5_ms:.2}"
        );
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];

        let Some(mono) =
            score_fixed_mono_consensus_candidate(&name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(hybrid) = score_fixed_hybrid_mono_consensus_candidate(
            &name,
            rendered,
            &mono_refs,
            &loaded_refs,
            &mono_target,
        ) else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(&name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(&name, rendered, &dynamics_targets)
        else {
            continue;
        };

        rows.push((
            name,
            mono.final_score,
            hybrid.final_score,
            side_cons.final_score,
            side_dyn.final_score,
        ));
    }

    rows.push((
        "current_default".to_owned(),
        score_fixed_mono_consensus_candidate(
            "current_default",
            anchor_rendered,
            &mono_refs,
            &mono_target,
        )
        .expect("expected mono score")
        .final_score,
        score_fixed_hybrid_mono_consensus_candidate(
            "current_default",
            anchor_rendered,
            &mono_refs,
            &loaded_refs,
            &mono_target,
        )
        .expect("expected hybrid score")
        .final_score,
        score_fixed_sectioned_side_consensus_candidate(
            "current_default",
            anchor_rendered,
            &side_targets,
        )
        .expect("expected side score")
        .final_score,
        score_fixed_sectioned_side_dynamics_candidate(
            "current_default",
            anchor_rendered,
            &dynamics_targets,
        )
        .expect("expected dynamics score")
        .final_score,
    ));

    rows.sort_by(|a, b| {
        b.4.partial_cmp(&a.4)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ side-memory transient-mix refine ===");
    for (name, mono, hybrid, side_cons, side_dyn) in rows {
        eprintln!(
            "{name:>42}: mono={mono:.4} hybrid={hybrid:.4} side={side_cons:.4} dyn={side_dyn:.4}"
        );
    }
}

#[test]
#[ignore = "opt-in GHZ/Sonic audio-tuning diagnostic; requires a local commercial Sonic ROM (see SONIC_ROM_PATH) and/or reference audio in tests/reference_audio/. Run with -- --ignored"]
fn diagnose_ghz_side_memory_sign_align_refine() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
            return;
        }
    };

    let refs = ghz_reference_paths();
    if refs.is_empty() {
        eprintln!("No GHZ references found, skipping");
        return;
    }

    let mut loaded_refs = Vec::new();
    let mut max_samples = 0usize;
    for path in refs {
        let (rate, samples) = load_reference(&path).expect("failed to load reference");
        if rate != SAMPLE_RATE {
            continue;
        }
        max_samples = max_samples.max(samples.len());
        loaded_refs.push((path, samples));
    }
    if loaded_refs.len() < 2 {
        eprintln!("Need at least two 44.1kHz GHZ references, skipping");
        return;
    }

    let record_frames = ((max_samples as f64 / (2.0 * SAMPLE_RATE as f64)) * 59.92) as u32;
    let record_frames = record_frames.max(300);
    let trace = capture_ghz_timed_trace(&rom, record_frames);
    let current_default = AudioOutputConfig::default();

    let mut anchor_renderer = CoreAudioRenderer::with_audio_output_config(current_default);
    let anchor_rendered_full =
        anchor_renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
    let capture_start = (trace.capture_start_sample as usize)
        .saturating_mul(2)
        .min(anchor_rendered_full.len());
    let anchor_rendered = &anchor_rendered_full[capture_start..];

    let Some((mono_target, mono_refs)) =
        build_fixed_mono_consensus_target(anchor_rendered, &loaded_refs)
    else {
        eprintln!("Unable to build fixed mono consensus target");
        return;
    };
    let Some(side_targets) =
        build_fixed_sectioned_side_consensus(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side consensus targets");
        return;
    };
    let Some(dynamics_targets) =
        build_fixed_sectioned_side_dynamics(anchor_rendered, &loaded_refs, SAMPLE_RATE as usize)
    else {
        eprintln!("Unable to build sectioned side dynamics targets");
        return;
    };

    let mut rows = Vec::new();
    for &(ch1_align, ch4_align, ch5_align, ch4_amount, ch5_amount) in &[
        (0.0f32, 0.25f32, 0.0f32, 0.60f32, 0.00f32),
        (0.0f32, 0.50f32, 0.0f32, 0.60f32, 0.00f32),
        (0.0f32, 0.75f32, 0.0f32, 0.60f32, 0.00f32),
        (0.0f32, 1.00f32, 0.0f32, 0.60f32, 0.00f32),
        (0.20f32, 0.75f32, 0.0f32, 0.60f32, 0.00f32),
        (0.20f32, 1.00f32, 0.0f32, 0.60f32, 0.00f32),
        (0.0f32, 0.75f32, 0.0f32, 0.65f32, 0.00f32),
        (0.0f32, 1.00f32, 0.0f32, 0.65f32, 0.00f32),
        (0.0f32, 0.75f32, 0.20f32, 0.60f32, 0.02f32),
        (0.0f32, 1.00f32, 0.20f32, 0.60f32, 0.02f32),
    ] {
        let config = current_default
            .with_ym_channel_side_memory_amounts([0.20, 0.0, 0.0, ch4_amount, ch5_amount, 0.0])
            .with_ym_channel_side_sign_align_mixes([
                ch1_align, 0.0, 0.0, ch4_align, ch5_align, 0.0,
            ]);
        let name = format!(
            "sa_{ch1_align:.2}_{ch4_align:.2}_{ch5_align:.2}_a_{ch4_amount:.2}_{ch5_amount:.2}"
        );
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let rendered_full =
            renderer.render_timed_writes(&trace.ym_writes, &trace.psg_writes, 0, trace.end_tick);
        let rendered = &rendered_full[capture_start..];

        let Some(mono) =
            score_fixed_mono_consensus_candidate(&name, rendered, &mono_refs, &mono_target)
        else {
            continue;
        };
        let Some(hybrid) = score_fixed_hybrid_mono_consensus_candidate(
            &name,
            rendered,
            &mono_refs,
            &loaded_refs,
            &mono_target,
        ) else {
            continue;
        };
        let Some(side_cons) =
            score_fixed_sectioned_side_consensus_candidate(&name, rendered, &side_targets)
        else {
            continue;
        };
        let Some(side_dyn) =
            score_fixed_sectioned_side_dynamics_candidate(&name, rendered, &dynamics_targets)
        else {
            continue;
        };

        rows.push((
            name,
            mono.final_score,
            hybrid.final_score,
            side_cons.final_score,
            side_dyn.final_score,
        ));
    }

    rows.push((
        "current_default".to_owned(),
        score_fixed_mono_consensus_candidate(
            "current_default",
            anchor_rendered,
            &mono_refs,
            &mono_target,
        )
        .expect("expected mono score")
        .final_score,
        score_fixed_hybrid_mono_consensus_candidate(
            "current_default",
            anchor_rendered,
            &mono_refs,
            &loaded_refs,
            &mono_target,
        )
        .expect("expected hybrid score")
        .final_score,
        score_fixed_sectioned_side_consensus_candidate(
            "current_default",
            anchor_rendered,
            &side_targets,
        )
        .expect("expected side score")
        .final_score,
        score_fixed_sectioned_side_dynamics_candidate(
            "current_default",
            anchor_rendered,
            &dynamics_targets,
        )
        .expect("expected dynamics score")
        .final_score,
    ));

    rows.sort_by(|a, b| {
        b.4.partial_cmp(&a.4)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    eprintln!("=== GHZ side-memory sign-align refine ===");
    for (name, mono, hybrid, side_cons, side_dyn) in rows {
        eprintln!(
            "{name:>34}: mono={mono:.4} hybrid={hybrid:.4} side={side_cons:.4} dyn={side_dyn:.4}"
        );
    }
}

//! Time-travel debugging: snapshot rewind with anchor + delta compression.
//!
//! # Design
//!
//! Naively keeping a full [`GenesisCoreSnapshot`] for every frame of a 30-second
//! window costs tens of megabytes. Instead we keep a small number of full
//! **keyframes** (anchors) plus per-frame **deltas** that record only the bytes
//! that changed since the previous frame. Reconstructing frame N means cloning
//! the latest keyframe at or before N and replaying the deltas up to N.
//!
//! The delta scheme and naming mirror the NES sibling emulator's rewind
//! implementation ([`ArrayDelta`], [`FieldDelta`], [`FrameDelta`],
//! [`KeyframePolicy`], [`CompressedTimeline`]).
//!
//! ## Divergence from the NES design (intentional)
//!
//! The NES keeps rewind in a standalone crate driven purely by the frontend,
//! with no `Command`/`CoreQuery` variants and no input log. This module is
//! instead embedded **inside `genesoxide-core`** so that `GenesisCore::execute`
//! can service `Command::Rewind`/`StepBack`/`SetRewindConfig` and report status
//! via accessors/`CoreQuery::RewindStatus`, and so we avoid a circular crate
//! dependency (a standalone crate would need to depend on the core for the
//! snapshot type, while the core needs the timeline). We additionally record
//! per-frame [`FrameInput`] in the timeline to support deterministic replay and
//! the determinism test. The anchor + delta internals are unchanged.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::api::{GenesisCoreSnapshot, Mapper};
use crate::rom::SramLayout;
use crate::scheduler::Region;
use crate::vdp::{AccessType, ControlState, VdpSnapshot};

/// NTSC frames per second, used to convert history seconds to a frame budget.
const FRAMES_PER_SECOND: u64 = 60;

/// EMA smoothing factor in Q8 fixed point (matches the NES keyframe policy).
const ALPHA_Q8: u32 = 32;

/// Rough per-frame overhead (bytes) charged for the small components stored in
/// a snapshot/delta beyond the explicitly-sized buffers. Used only for the
/// memory-usage estimate reported by [`RewindStatus`].
const SMALL_COMPONENT_BYTES: usize = 2048;

// ── Array delta (RLE byte diff) ─────────────────────────────────────────────

/// A run of changed bytes in a large array: `data` should be written back
/// starting at `offset`.
///
/// The NES used `offset: u16` + `SmallVec`; Genesis buffers reach 64KB so we
/// use `offset: u32` and a plain `Vec<u8>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArrayDelta {
    /// Byte offset into the target array where this run begins.
    pub offset: u32,
    /// The new bytes for `target[offset..offset + data.len()]`.
    pub data: Vec<u8>,
}

/// Computes the runs of bytes that differ between `before` and `after`,
/// coalescing adjacent changed bytes into a single run.
///
/// This is a plain diff (stores the new bytes), not an XOR. The two slices are
/// expected to be the same length (fixed-size hardware buffers); any trailing
/// bytes in `after` beyond `before` are emitted as a final run.
#[must_use]
pub fn diff_array(before: &[u8], after: &[u8]) -> Vec<ArrayDelta> {
    let mut deltas = Vec::new();
    let common = before.len().min(after.len());
    let mut i = 0;
    while i < common {
        if before[i] != after[i] {
            let start = i;
            while i < common && before[i] != after[i] {
                i += 1;
            }
            deltas.push(ArrayDelta {
                offset: start as u32,
                data: after[start..i].to_vec(),
            });
        } else {
            i += 1;
        }
    }
    // Trailing growth (should not happen for fixed-size buffers, handled anyway).
    if after.len() > common {
        deltas.push(ArrayDelta {
            offset: common as u32,
            data: after[common..].to_vec(),
        });
    }
    deltas
}

/// Applies previously-computed [`ArrayDelta`]s back onto `target`.
pub fn apply_deltas(target: &mut [u8], deltas: &[ArrayDelta]) {
    for delta in deltas {
        let start = delta.offset as usize;
        let end = start + delta.data.len();
        if end <= target.len() {
            target[start..end].copy_from_slice(&delta.data);
        }
    }
}

// ── VDP meta (VDP state minus the VRAM byte buffer) ─────────────────────────

/// The non-VRAM portion of the VDP state. VRAM is diffed as a byte array; the
/// rest (which is small and tends to change together) is stored whole when any
/// of it changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VdpMeta {
    pub cram: Vec<u16>,
    pub vsram: Vec<u16>,
    pub registers: Vec<u8>,
    pub control_state: ControlState,
    pub address: u16,
    pub scanline: u16,
    pub dot: u16,
    pub auto_increment: u16,
    pub access_type: Option<AccessType>,
    pub in_vblank: bool,
    pub in_hblank: bool,
    pub dma_pending: bool,
    pub dma_fill_pending: bool,
    pub h_interrupt_counter: i16,
    pub h_interrupt_pending: bool,
    pub control_code: u8,
    pub odd_frame: bool,
    pub vint_pending: bool,
    pub dma_busy_cpu_cycles: u32,
    pub region: Region,
}

impl VdpMeta {
    fn from_snapshot(v: &VdpSnapshot) -> Self {
        Self {
            cram: v.cram.clone(),
            vsram: v.vsram.clone(),
            registers: v.registers.clone(),
            control_state: v.control_state,
            address: v.address,
            scanline: v.scanline,
            dot: v.dot,
            auto_increment: v.auto_increment,
            access_type: v.access_type,
            in_vblank: v.in_vblank,
            in_hblank: v.in_hblank,
            dma_pending: v.dma_pending,
            dma_fill_pending: v.dma_fill_pending,
            h_interrupt_counter: v.h_interrupt_counter,
            h_interrupt_pending: v.h_interrupt_pending,
            control_code: v.control_code,
            odd_frame: v.odd_frame,
            vint_pending: v.vint_pending,
            dma_busy_cpu_cycles: v.dma_busy_cpu_cycles,
            region: v.region,
        }
    }

    fn apply_to(&self, v: &mut VdpSnapshot) {
        v.cram = self.cram.clone();
        v.vsram = self.vsram.clone();
        v.registers = self.registers.clone();
        v.control_state = self.control_state;
        v.address = self.address;
        v.scanline = self.scanline;
        v.dot = self.dot;
        v.auto_increment = self.auto_increment;
        v.access_type = self.access_type;
        v.in_vblank = self.in_vblank;
        v.in_hblank = self.in_hblank;
        v.dma_pending = self.dma_pending;
        v.dma_fill_pending = self.dma_fill_pending;
        v.h_interrupt_counter = self.h_interrupt_counter;
        v.h_interrupt_pending = self.h_interrupt_pending;
        v.control_code = self.control_code;
        v.odd_frame = self.odd_frame;
        v.vint_pending = self.vint_pending;
        v.dma_busy_cpu_cycles = self.dma_busy_cpu_cycles;
        v.region = self.region;
    }

    fn estimated_bytes(&self) -> usize {
        self.cram.len() * 2 + self.vsram.len() * 2 + self.registers.len() + 32
    }
}

// ── Small scalars bundle ────────────────────────────────────────────────────

/// The scalar core fields (bundled so they are diffed as one unit).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScalarState {
    pub z80_bank: u32,
    pub z80_bus_requested: bool,
    pub z80_reset: bool,
    pub z80_reset_pending: bool,
    pub z80_bus_released_this_scanline: bool,
    pub frame_count: u64,
    pub speed_permille: u16,
    pub paused: bool,
    pub region: Region,
    pub overseas: bool,
    pub region_override: Option<Region>,
    // Cartridge SRAM scalar flags (the `data` buffer is byte-diffed separately).
    pub sram_enabled: bool,
    pub sram_has_battery: bool,
    pub sram_header_declared: bool,
    pub sram_touched: bool,
    pub sram_dirty: bool,
    pub sram_start: u32,
    pub sram_end: u32,
    pub sram_layout: SramLayout,
}

impl ScalarState {
    fn from_snapshot(s: &GenesisCoreSnapshot) -> Self {
        Self {
            z80_bank: s.z80_bank,
            z80_bus_requested: s.z80_bus_requested,
            z80_reset: s.z80_reset,
            z80_reset_pending: s.z80_reset_pending,
            z80_bus_released_this_scanline: s.z80_bus_released_this_scanline,
            frame_count: s.frame_count,
            speed_permille: s.speed_permille,
            paused: s.paused,
            region: s.region,
            overseas: s.overseas,
            region_override: s.region_override,
            sram_enabled: s.sram.enabled,
            sram_has_battery: s.sram.has_battery,
            sram_header_declared: s.sram.header_declared,
            sram_touched: s.sram.touched,
            sram_dirty: s.sram.dirty,
            sram_start: s.sram.start,
            sram_end: s.sram.end,
            sram_layout: s.sram.layout,
        }
    }

    fn apply_to(&self, s: &mut GenesisCoreSnapshot) {
        s.z80_bank = self.z80_bank;
        s.z80_bus_requested = self.z80_bus_requested;
        s.z80_reset = self.z80_reset;
        s.z80_reset_pending = self.z80_reset_pending;
        s.z80_bus_released_this_scanline = self.z80_bus_released_this_scanline;
        s.frame_count = self.frame_count;
        s.speed_permille = self.speed_permille;
        s.paused = self.paused;
        s.region = self.region;
        s.overseas = self.overseas;
        s.region_override = self.region_override;
        s.sram.enabled = self.sram_enabled;
        s.sram.has_battery = self.sram_has_battery;
        s.sram.header_declared = self.sram_header_declared;
        s.sram.touched = self.sram_touched;
        s.sram.dirty = self.sram_dirty;
        s.sram.start = self.sram_start;
        s.sram.end = self.sram_end;
        s.sram.layout = self.sram_layout;
    }
}

// ── Field delta (Option-per-component scalar diff) ──────────────────────────

/// The small (non-byte-array) components of a snapshot, each present only if it
/// changed relative to the previous frame.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FieldDelta {
    pub cpu: Option<crate::cpu::CpuSnapshot>,
    pub scheduler: Option<crate::scheduler::SchedulerSnapshot>,
    pub z80: Option<crate::z80::Z80Snapshot>,
    pub psg: Option<crate::psg::Psg>,
    pub ym2612: Option<crate::ym2612::Ym2612>,
    pub port1: Option<crate::io::ControllerPort>,
    pub port2: Option<crate::io::ControllerPort>,
    pub vdp_meta: Option<VdpMeta>,
    pub scalars: Option<ScalarState>,
    pub mapper: Option<Mapper>,
}

impl FieldDelta {
    fn compute(before: &GenesisCoreSnapshot, after: &GenesisCoreSnapshot) -> Self {
        Self {
            cpu: (before.cpu != after.cpu).then(|| after.cpu.clone()),
            mapper: (before.mapper != after.mapper).then(|| after.mapper.clone()),
            scheduler: (before.scheduler != after.scheduler).then(|| after.scheduler.clone()),
            z80: (before.z80 != after.z80).then(|| after.z80.clone()),
            psg: (before.psg != after.psg).then(|| after.psg.clone()),
            ym2612: (before.ym2612 != after.ym2612).then(|| after.ym2612.clone()),
            port1: (before.port1 != after.port1).then(|| after.port1.clone()),
            port2: (before.port2 != after.port2).then(|| after.port2.clone()),
            vdp_meta: {
                let b = VdpMeta::from_snapshot(&before.vdp);
                let a = VdpMeta::from_snapshot(&after.vdp);
                (b != a).then_some(a)
            },
            scalars: {
                let b = ScalarState::from_snapshot(before);
                let a = ScalarState::from_snapshot(after);
                (b != a).then_some(a)
            },
        }
    }

    fn apply(&self, target: &mut GenesisCoreSnapshot) {
        if let Some(v) = &self.cpu {
            target.cpu = v.clone();
        }
        if let Some(v) = &self.mapper {
            target.mapper = v.clone();
        }
        if let Some(v) = &self.scheduler {
            target.scheduler = v.clone();
        }
        if let Some(v) = &self.z80 {
            target.z80 = v.clone();
        }
        if let Some(v) = &self.psg {
            target.psg = v.clone();
        }
        if let Some(v) = &self.ym2612 {
            target.ym2612 = v.clone();
        }
        if let Some(v) = &self.port1 {
            target.port1 = v.clone();
        }
        if let Some(v) = &self.port2 {
            target.port2 = v.clone();
        }
        if let Some(v) = &self.vdp_meta {
            v.apply_to(&mut target.vdp);
        }
        if let Some(v) = &self.scalars {
            v.apply_to(target);
        }
    }

    fn estimated_bytes(&self) -> usize {
        let mut n = 0;
        if self.cpu.is_some() {
            n += 64;
        }
        if self.mapper.is_some() {
            n += 16;
        }
        if self.scheduler.is_some() {
            n += 24;
        }
        if self.z80.is_some() {
            n += 40;
        }
        if self.psg.is_some() {
            n += 64;
        }
        if self.ym2612.is_some() {
            n += 1024;
        }
        if self.port1.is_some() {
            n += 8;
        }
        if self.port2.is_some() {
            n += 8;
        }
        if let Some(v) = &self.vdp_meta {
            n += v.estimated_bytes();
        }
        if self.scalars.is_some() {
            n += 24;
        }
        n
    }
}

// ── Per-frame input log ─────────────────────────────────────────────────────

/// Controller input recorded for a frame, enabling deterministic replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FrameInput {
    pub port1_buttons: u16,
    pub port2_buttons: u16,
}

// ── Frame delta ─────────────────────────────────────────────────────────────

/// A per-frame delta: the byte runs that changed in each large buffer plus the
/// small components that changed, tagged with the frame id and the input that
/// produced this frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameDelta {
    pub frame_id: u64,
    pub work_ram_deltas: Vec<ArrayDelta>,
    pub vram_deltas: Vec<ArrayDelta>,
    pub z80_ram_deltas: Vec<ArrayDelta>,
    /// Byte diff of the cartridge SRAM backing buffer (`sram.data`). Empty when
    /// SRAM is absent/unchanged; a full run is emitted when the buffer length
    /// changes (e.g. a different ROM's SRAM window) so the target is resized.
    pub sram_deltas: Vec<ArrayDelta>,
    /// Target length of `sram.data` after this frame. Lets `apply` resize the
    /// buffer before writing the byte runs so a length change (or an
    /// initially-empty SRAM) reconstructs exactly rather than being clamped.
    pub sram_len: u32,
    pub fields: FieldDelta,
    pub input: FrameInput,
    /// Sum of changed-byte-run lengths across all buffers; drives the keyframe
    /// promotion policy.
    pub compressed_size: u32,
}

impl FrameDelta {
    /// Diffs `before` → `after`, producing a delta for `after`'s frame id.
    #[must_use]
    pub fn compute(
        before: &GenesisCoreSnapshot,
        after: &GenesisCoreSnapshot,
        input: FrameInput,
    ) -> Self {
        let work_ram_deltas = diff_array(&before.work_ram, &after.work_ram);
        let vram_deltas = diff_array(&before.vdp.vram, &after.vdp.vram);
        let z80_ram_deltas = diff_array(&before.z80_ram, &after.z80_ram);
        let sram_deltas = diff_array(&before.sram.data, &after.sram.data);
        let fields = FieldDelta::compute(before, after);

        let run_bytes = |ds: &[ArrayDelta]| ds.iter().map(|d| d.data.len()).sum::<usize>();
        let compressed_size = (run_bytes(&work_ram_deltas)
            + run_bytes(&vram_deltas)
            + run_bytes(&z80_ram_deltas)
            + run_bytes(&sram_deltas)) as u32;

        Self {
            frame_id: after.frame_count,
            work_ram_deltas,
            vram_deltas,
            z80_ram_deltas,
            sram_deltas,
            sram_len: after.sram.data.len() as u32,
            fields,
            input,
            compressed_size,
        }
    }

    /// Applies this delta onto `target`, advancing it to this frame's state.
    pub fn apply(&self, target: &mut GenesisCoreSnapshot) {
        apply_deltas(&mut target.work_ram, &self.work_ram_deltas);
        apply_deltas(&mut target.vdp.vram, &self.vram_deltas);
        apply_deltas(&mut target.z80_ram, &self.z80_ram_deltas);
        // Resize the SRAM buffer to this frame's length before applying the
        // byte runs so growth/shrink (and an initially-empty buffer) reconstruct
        // exactly instead of being clamped by `apply_deltas`' bounds check.
        target.sram.data.resize(self.sram_len as usize, 0);
        apply_deltas(&mut target.sram.data, &self.sram_deltas);
        self.fields.apply(target);
    }

    /// Estimated heap cost of storing this delta, for status reporting.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        self.compressed_size as usize + self.fields.estimated_bytes()
    }
}

// ── Keyframe policy ─────────────────────────────────────────────────────────

/// Decides when a frame should be promoted to a full keyframe instead of a
/// delta. Promotes on a fixed interval or when a delta is a large spike.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyframePolicy {
    pub base_interval: u64,
    pub spike_threshold: u32,
    pub frames_since_keyframe: u64,
    pub rolling_avg: u32,
}

impl KeyframePolicy {
    #[must_use]
    pub fn new(base_interval: u64, spike_threshold: u32) -> Self {
        Self {
            base_interval: base_interval.max(1),
            spike_threshold,
            frames_since_keyframe: 0,
            rolling_avg: 0,
        }
    }

    /// Records a delta of `delta_size` bytes and returns whether the current
    /// frame should be a keyframe. Updates the rolling average (EMA) and the
    /// frames-since-keyframe counter; resets the counter on promotion.
    pub fn should_promote(&mut self, delta_size: u32) -> bool {
        self.frames_since_keyframe += 1;

        // EMA update in Q8 fixed point.
        let weighted_new = (u64::from(delta_size) * u64::from(ALPHA_Q8)) >> 8;
        let weighted_old = (u64::from(self.rolling_avg) * u64::from(256 - ALPHA_Q8)) >> 8;
        self.rolling_avg = (weighted_new + weighted_old) as u32;

        let interval_hit = self.frames_since_keyframe >= self.base_interval;
        let spike = delta_size > self.spike_threshold
            && u64::from(delta_size) > u64::from(self.rolling_avg) * 3;

        if interval_hit || spike {
            self.frames_since_keyframe = 0;
            true
        } else {
            false
        }
    }
}

// ── Compressed timeline ─────────────────────────────────────────────────────

/// A full snapshot anchor stored at a frame boundary.
#[derive(Debug, Clone)]
pub struct Keyframe {
    pub frame_id: u64,
    pub snapshot: GenesisCoreSnapshot,
}

fn snapshot_estimated_bytes(s: &GenesisCoreSnapshot) -> usize {
    s.work_ram.len()
        + s.z80_ram.len()
        + s.vdp.vram.len()
        + s.vdp.cram.len() * 2
        + s.vdp.vsram.len() * 2
        + s.vdp.registers.len()
        + SMALL_COMPONENT_BYTES
}

/// The anchor + delta ring: a bounded window of keyframes and per-frame deltas.
#[derive(Debug, Clone)]
pub struct CompressedTimeline {
    keyframes: VecDeque<Keyframe>,
    deltas: VecDeque<FrameDelta>,
    /// Frame-slot budget: `keyframes.len() + deltas.len()` is kept `<= max_frames`.
    max_frames: u64,
    policy: KeyframePolicy,
    last_snapshot: Option<GenesisCoreSnapshot>,
    last_frame_id: Option<u64>,
}

impl CompressedTimeline {
    #[must_use]
    pub fn new(max_frames: u64, policy: KeyframePolicy) -> Self {
        Self {
            keyframes: VecDeque::new(),
            deltas: VecDeque::new(),
            max_frames: max_frames.max(1),
            policy,
            last_snapshot: None,
            last_frame_id: None,
        }
    }

    /// Number of stored frame slots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keyframes.len() + self.deltas.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Records `snapshot` (for `frame_id`) with the input that produced it.
    ///
    /// The first frame is always a keyframe; subsequent frames become deltas
    /// unless the policy promotes them. If `frame_id` is not strictly greater
    /// than the last recorded frame (i.e. we are re-recording after a rewind),
    /// the forward history is truncated first.
    pub fn push(&mut self, frame_id: u64, snapshot: GenesisCoreSnapshot, input: FrameInput) {
        if let Some(last) = self.last_frame_id {
            if frame_id <= last {
                // Re-recording over abandoned future: drop everything at or
                // after this frame so the new state overwrites it.
                self.truncate_after(frame_id.saturating_sub(1));
            }
        }

        match &self.last_snapshot {
            Some(prev) => {
                let delta = FrameDelta::compute(prev, &snapshot, input);
                if self.policy.should_promote(delta.compressed_size) {
                    self.keyframes.push_back(Keyframe {
                        frame_id,
                        snapshot: snapshot.clone(),
                    });
                } else {
                    self.deltas.push_back(delta);
                }
            }
            None => {
                self.keyframes.push_back(Keyframe {
                    frame_id,
                    snapshot: snapshot.clone(),
                });
            }
        }

        self.last_snapshot = Some(snapshot);
        self.last_frame_id = Some(frame_id);
        self.prune();
    }

    /// Reconstructs the snapshot for `target_frame_id`, or `None` if it is
    /// outside the retained window.
    #[must_use]
    pub fn reconstruct(&self, target_frame_id: u64) -> Option<GenesisCoreSnapshot> {
        match self.last_frame_id {
            Some(last) if target_frame_id <= last => {}
            _ => return None,
        }

        // Latest keyframe with frame_id <= target.
        let kf = self
            .keyframes
            .iter()
            .filter(|k| k.frame_id <= target_frame_id)
            .max_by_key(|k| k.frame_id)?;

        let mut state = kf.snapshot.clone();
        for delta in &self.deltas {
            if delta.frame_id > kf.frame_id && delta.frame_id <= target_frame_id {
                delta.apply(&mut state);
            }
        }
        Some(state)
    }

    /// Drops all keyframes and deltas with `frame_id > target`, and resets the
    /// "last" cursor to the reconstructed state at `target`.
    pub fn truncate_after(&mut self, target: u64) {
        self.keyframes.retain(|k| k.frame_id <= target);
        self.deltas.retain(|d| d.frame_id <= target);
        self.last_frame_id = self.newest_frame();
        self.last_snapshot = self
            .last_frame_id
            .and_then(|f| self.reconstruct(f));
        if self.last_snapshot.is_none() {
            self.last_frame_id = None;
        }
    }

    /// Removes the oldest frame slots until within the `max_frames` budget.
    ///
    /// When the oldest delta belongs to the first keyframe's segment it is
    /// absorbed into that keyframe in place (advancing the keyframe forward);
    /// otherwise the oldest keyframe and any deltas orphaned before the next
    /// keyframe are dropped.
    fn prune(&mut self) {
        while self.len() as u64 > self.max_frames {
            let oldest_kf_frame = self.keyframes.front().map(|k| k.frame_id);
            let second_kf_frame = self.keyframes.iter().nth(1).map(|k| k.frame_id);
            let oldest_delta_frame = self.deltas.front().map(|d| d.frame_id);

            match (oldest_kf_frame, oldest_delta_frame) {
                (Some(kf0), Some(d0))
                    if d0 > kf0 && second_kf_frame.map_or(true, |kf1| d0 < kf1) =>
                {
                    // Oldest delta belongs to the first keyframe's segment:
                    // absorb it into the keyframe in place.
                    let delta = self.deltas.pop_front().expect("checked above");
                    if let Some(front) = self.keyframes.front_mut() {
                        delta.apply(&mut front.snapshot);
                        front.frame_id = delta.frame_id;
                    }
                }
                (Some(_), _) => {
                    // Drop the oldest keyframe and any deltas orphaned before
                    // the next keyframe (they can no longer be reconstructed).
                    self.keyframes.pop_front();
                    let next_kf = self.keyframes.front().map(|k| k.frame_id);
                    while let Some(front) = self.deltas.front() {
                        match next_kf {
                            Some(nk) if front.frame_id >= nk => break,
                            _ => {
                                self.deltas.pop_front();
                            }
                        }
                    }
                }
                _ => {
                    // No keyframes left; nothing sensibly reconstructable.
                    self.deltas.pop_front();
                }
            }
        }
    }

    /// Oldest reconstructable frame id.
    #[must_use]
    pub fn oldest_frame(&self) -> Option<u64> {
        self.keyframes.front().map(|k| k.frame_id)
    }

    /// Newest recorded frame id.
    #[must_use]
    pub fn newest_frame(&self) -> Option<u64> {
        let kf = self.keyframes.back().map(|k| k.frame_id);
        let df = self.deltas.back().map(|d| d.frame_id);
        match (kf, df) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    /// Estimated heap bytes used by the stored keyframes and deltas.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        let kf: usize = self
            .keyframes
            .iter()
            .map(|k| snapshot_estimated_bytes(&k.snapshot))
            .sum();
        let df: usize = self.deltas.iter().map(FrameDelta::estimated_bytes).sum();
        kf + df
    }

    /// Returns the input recorded for `frame_id`, if it was stored as a delta.
    #[must_use]
    pub fn input_at(&self, frame_id: u64) -> Option<FrameInput> {
        self.deltas
            .iter()
            .find(|d| d.frame_id == frame_id)
            .map(|d| d.input)
    }
}

// ── Runtime configuration and status ────────────────────────────────────────

/// Runtime rewind configuration, applied via `Command::SetRewindConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindConfig {
    /// Whether rewind recording is active.
    pub enabled: bool,
    /// Fixed keyframe promotion interval, in frames.
    pub keyframe_base_interval: u64,
    /// How many seconds of history to retain.
    pub max_history_seconds: u32,
    /// Delta size (bytes) above which a spike keyframe may be promoted.
    pub delta_spike_threshold: u32,
}

impl Default for RewindConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            keyframe_base_interval: 60,
            max_history_seconds: 30,
            delta_spike_threshold: 2048,
        }
    }
}

impl RewindConfig {
    /// Frame-slot budget derived from the history window.
    #[must_use]
    pub fn max_frames(&self) -> u64 {
        (u64::from(self.max_history_seconds) * FRAMES_PER_SECOND).max(1)
    }
}

/// A read-only view of the rewind buffer state, for UI/telemetry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindStatus {
    pub enabled: bool,
    pub frames_available: u64,
    pub memory_used_bytes: usize,
    pub oldest_frame: Option<u64>,
    pub newest_frame: Option<u64>,
}

// ── Rewind buffer (the public, synchronous, in-core facade) ─────────────────

/// The in-core rewind facade. Owns the compressed timeline and the active
/// configuration, and is driven synchronously from `GenesisCore`.
#[derive(Debug, Clone)]
pub struct RewindBuffer {
    pub config: RewindConfig,
    timeline: CompressedTimeline,
}

impl RewindBuffer {
    #[must_use]
    pub fn new(config: RewindConfig) -> Self {
        let policy = KeyframePolicy::new(config.keyframe_base_interval, config.delta_spike_threshold);
        let timeline = CompressedTimeline::new(config.max_frames(), policy);
        Self { config, timeline }
    }

    /// Records a frame snapshot with its input. No-op when disabled.
    pub fn record(&mut self, frame_id: u64, snapshot: GenesisCoreSnapshot, input: FrameInput) {
        if !self.config.enabled {
            return;
        }
        self.timeline.push(frame_id, snapshot, input);
    }

    /// Reconstructs the state at `frame_id`, if retained.
    #[must_use]
    pub fn reconstruct(&self, frame_id: u64) -> Option<GenesisCoreSnapshot> {
        self.timeline.reconstruct(frame_id)
    }

    /// Truncates any recorded history newer than `frame_id` (used after a
    /// rewind, so re-running overwrites the abandoned future).
    pub fn truncate_after(&mut self, frame_id: u64) {
        self.timeline.truncate_after(frame_id);
    }

    /// Applies a new configuration. Reconfigures the timeline budget and
    /// policy; clears the buffer if rewind was disabled.
    pub fn set_config(&mut self, config: RewindConfig) {
        let policy =
            KeyframePolicy::new(config.keyframe_base_interval, config.delta_spike_threshold);
        if !config.enabled {
            self.timeline = CompressedTimeline::new(config.max_frames(), policy);
        } else {
            let mut timeline = CompressedTimeline::new(config.max_frames(), policy);
            std::mem::swap(&mut timeline, &mut self.timeline);
            // Preserve existing keyframes/deltas by moving them into the new
            // timeline, then re-prune to the possibly-smaller budget.
            self.timeline.keyframes = timeline.keyframes;
            self.timeline.deltas = timeline.deltas;
            self.timeline.last_snapshot = timeline.last_snapshot;
            self.timeline.last_frame_id = timeline.last_frame_id;
            self.timeline.prune();
        }
        self.config = config;
    }

    /// Current status snapshot.
    #[must_use]
    pub fn status(&self) -> RewindStatus {
        let oldest = self.timeline.oldest_frame();
        let newest = self.timeline.newest_frame();
        let frames_available = match (oldest, newest) {
            (Some(o), Some(n)) => n.saturating_sub(o) + 1,
            _ => 0,
        };
        RewindStatus {
            enabled: self.config.enabled,
            frames_available,
            memory_used_bytes: self.timeline.estimated_bytes(),
            oldest_frame: oldest,
            newest_frame: newest,
        }
    }

    /// Number of retained frame slots (keyframes + deltas).
    #[must_use]
    pub fn frames_available(&self) -> u64 {
        self.status().frames_available
    }

    /// Estimated bytes of retained history.
    #[must_use]
    pub fn memory_used(&self) -> usize {
        self.timeline.estimated_bytes()
    }
}

impl Default for RewindBuffer {
    fn default() -> Self {
        Self::new(RewindConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_and_apply_roundtrip() {
        let before = vec![0u8; 256];
        let mut after = before.clone();
        after[10] = 1;
        after[11] = 2;
        after[100] = 9;
        after[200] = 7;
        after[201] = 8;

        let deltas = diff_array(&before, &after);
        // Three coalesced runs: [10..12], [100..101], [200..202].
        assert_eq!(deltas.len(), 3);
        assert_eq!(deltas[0].offset, 10);
        assert_eq!(deltas[0].data, vec![1, 2]);

        let mut target = before.clone();
        apply_deltas(&mut target, &deltas);
        assert_eq!(target, after);
    }

    #[test]
    fn diff_identical_is_empty() {
        let a = vec![5u8; 64];
        assert!(diff_array(&a, &a).is_empty());
    }

    #[test]
    fn policy_promotes_on_interval() {
        let mut policy = KeyframePolicy::new(4, 1_000_000);
        assert!(!policy.should_promote(10));
        assert!(!policy.should_promote(10));
        assert!(!policy.should_promote(10));
        assert!(policy.should_promote(10)); // 4th frame hits interval
        assert!(!policy.should_promote(10)); // counter reset
    }

    #[test]
    fn policy_promotes_on_spike() {
        let mut policy = KeyframePolicy::new(1_000_000, 100);
        // Warm up a low rolling average.
        for _ in 0..10 {
            assert!(!policy.should_promote(10));
        }
        // A big spike, above threshold and > 3x the average, promotes.
        assert!(policy.should_promote(5_000));
    }

    /// Produces a sequence of per-frame snapshots from a fresh core.
    fn snapshot_sequence(frames: usize) -> Vec<(u64, GenesisCoreSnapshot)> {
        use crate::{Command, GenesisCore};
        let mut core = GenesisCore::new();
        core.execute(Command::LoadRom(vec![0u8; 0x8000]));
        let mut out = Vec::new();
        for _ in 0..frames {
            core.execute(Command::StepFrame);
            out.push((core.frame_count(), core.snapshot()));
        }
        out
    }

    #[test]
    fn frame_delta_compute_apply_matches() {
        let seq = snapshot_sequence(4);
        let before = &seq[1].1;
        let after = &seq[3].1;
        let delta = FrameDelta::compute(before, after, FrameInput::default());
        let mut target = before.clone();
        delta.apply(&mut target);
        assert!(target == *after, "FrameDelta apply did not reproduce state");
    }

    /// Regression guard for PRs #7–#13: the hand-written delta encoder must
    /// carry the newly-added persistent state (SRAM, SSF2 mapper banks,
    /// VInt-pending, DMA-busy cycles, region/overseas/region_override). Before
    /// the fix these fields had no representation in the delta path, so a delta
    /// frame reconstructed them at stale keyframe values.
    #[test]
    fn delta_carries_new_persistent_state() {
        // Start from a real, internally-consistent snapshot.
        let seq = snapshot_sequence(1);
        let keyframe = seq[0].1.clone();

        // Build a target frame in which every one of the new fields differs
        // from the keyframe.
        let mut target = keyframe.clone();
        target.vdp.vint_pending = true;
        target.vdp.dma_busy_cpu_cycles = 12_345;
        target.vdp.region = Region::Pal;
        target.region = Region::Pal;
        target.overseas = false;
        target.region_override = Some(Region::Pal);
        target.mapper = Mapper::Ssf2 {
            banks: [7, 6, 5, 4, 3, 2, 1, 0],
        };
        // SRAM: flip the scalar flags and mutate a backing byte.
        assert!(
            !target.sram.data.is_empty(),
            "test setup expects a non-empty SRAM buffer",
        );
        target.sram.enabled = true;
        target.sram.touched = true;
        target.sram.dirty = true;
        let sram_idx = target.sram.data.len() / 2;
        target.sram.data[sram_idx] ^= 0xAB;

        // Sanity: the keyframe really does differ (otherwise the test is vacuous).
        assert_ne!(keyframe.vdp.vint_pending, target.vdp.vint_pending);
        assert_ne!(keyframe.mapper, target.mapper);
        assert_ne!(keyframe.sram.data, target.sram.data);

        // Compute the delta and replay it exactly as `reconstruct` does.
        let delta = FrameDelta::compute(&keyframe, &target, FrameInput::default());
        let mut recon = keyframe.clone();
        delta.apply(&mut recon);

        // Every new field must reconstruct to the target value, not the stale
        // keyframe value.
        assert_eq!(recon.vdp.vint_pending, target.vdp.vint_pending);
        assert_eq!(recon.vdp.dma_busy_cpu_cycles, target.vdp.dma_busy_cpu_cycles);
        assert_eq!(recon.vdp.region, target.vdp.region);
        assert_eq!(recon.region, target.region);
        assert_eq!(recon.overseas, target.overseas);
        assert_eq!(recon.region_override, target.region_override);
        assert_eq!(recon.mapper, target.mapper);
        assert_eq!(recon.sram.enabled, target.sram.enabled);
        assert_eq!(recon.sram.touched, target.sram.touched);
        assert_eq!(recon.sram.dirty, target.sram.dirty);
        assert_eq!(recon.sram.data, target.sram.data);

        // And the whole snapshot round-trips, proving nothing else drifted.
        assert!(recon == target, "full snapshot mismatch after delta replay");
    }

    #[test]
    fn timeline_reconstructs_every_retained_frame() {
        let seq = snapshot_sequence(40);
        let policy = KeyframePolicy::new(8, 1 << 30);
        let mut tl = CompressedTimeline::new(1000, policy);
        for (fid, snap) in &seq {
            tl.push(*fid, snap.clone(), FrameInput::default());
        }
        // Every recorded frame reconstructs exactly.
        for (fid, snap) in &seq {
            let recon = tl.reconstruct(*fid).expect("frame retained");
            assert!(recon == *snap, "frame {fid} reconstruct mismatch");
        }
        // Out-of-window frames yield None.
        assert!(tl.reconstruct(seq.last().unwrap().0 + 1).is_none());
    }

    #[test]
    fn timeline_respects_capacity() {
        let seq = snapshot_sequence(60);
        let policy = KeyframePolicy::new(8, 1 << 30);
        let max_frames = 20;
        let mut tl = CompressedTimeline::new(max_frames, policy);
        for (fid, snap) in &seq {
            tl.push(*fid, snap.clone(), FrameInput::default());
            assert!(tl.len() as u64 <= max_frames, "exceeded frame budget");
        }
        // The most recent frames are still reconstructable.
        let newest = tl.newest_frame().unwrap();
        let recon = tl.reconstruct(newest).expect("newest retained");
        assert!(recon == seq.last().unwrap().1);
    }

    #[test]
    fn timeline_truncate_after_drops_future() {
        let seq = snapshot_sequence(20);
        let policy = KeyframePolicy::new(4, 1 << 30);
        let mut tl = CompressedTimeline::new(1000, policy);
        for (fid, snap) in &seq {
            tl.push(*fid, snap.clone(), FrameInput::default());
        }
        let target = seq[9].0;
        tl.truncate_after(target);
        assert_eq!(tl.newest_frame(), Some(target));
        assert!(tl.reconstruct(target).is_some());
        assert!(tl.reconstruct(seq[10].0).is_none());
        // Reconstruct at target still matches.
        assert!(tl.reconstruct(target).unwrap() == seq[9].1);
    }
}

//! Sega Genesis VDP (Video Display Processor) — Yamaha YM7101.
//!
//! The VDP handles all video output: two scrollable tile planes (A and B),
//! a window plane, up to 80 sprites, and a DMA engine for bulk memory
//! transfers. Output resolution is 320x224 (NTSC H40 mode).
//!
//! Communication with the 68000 happens through two 16-bit ports:
//! - Data port (0xC00000): read/write VRAM, CRAM, VSRAM
//! - Control port (0xC00004): register writes, DMA setup, address setup
//!
//! The control port uses a two-word command sequence to set up the
//! destination and access type. This state machine is one of the
//! trickiest parts of Genesis emulation.

use serde::{Deserialize, Serialize};

use crate::api::{FRAME_RGBA_BYTES_MAX, FRAME_WIDTH};
use crate::scheduler::Region;
use crate::timing::MASTER_TICKS_PER_LINE_H40;

/// Active display height in V28 mode (NTSC and PAL default).
const ACTIVE_HEIGHT_V28: usize = 224;
/// Active display height in V30 mode (PAL only, reg 0x01 bit 3 set).
const ACTIVE_HEIGHT_V30: usize = 240;

/// Number of distinct 8-bit HCounter values across one H40 (320px) line.
///
/// The counter runs `0x00..=0xB6` (183 values) then jumps to `0xE4..=0xFF`
/// (28 values): 183 + 28 = 211. Source: Charles MacDonald, "Sega Genesis VDP
/// documentation" (genvdp.txt), and the Kabuto hardware notes mirrored at
/// <https://plutiedev.com/mirror/kabuto-hardware-notes> (both give H40 HCounter
/// `0x00-0xB6, 0xE4-0xFF`).
const HCOUNTER_STEPS_H40: u64 = 211;

/// Number of distinct 8-bit HCounter values across one H32 (256px) line.
///
/// The counter runs `0x00..=0x93` (148 values) then jumps to `0xE9..=0xFF`
/// (23 values): 148 + 23 = 171. Same sources as [`HCOUNTER_STEPS_H40`] (H32
/// HCounter `0x00-0x93, 0xE9-0xFF`).
const HCOUNTER_STEPS_H32: u64 = 171;

/// Maps an intra-line master-tick offset to the documented 8-bit HCounter value.
///
/// `offset_ticks` is the beam's position within the current scanline, measured
/// in master-clock ticks from the line's start (`0..MASTER_TICKS_PER_LINE_H40`).
/// The returned value follows the hardware HCounter progression with its
/// mid-line jump:
/// - H40: `0x00..=0xB6` then `0xE4..=0xFF`
/// - H32: `0x00..=0x93` then `0xE9..=0xFF`
///
/// Source: Charles MacDonald "Sega Genesis VDP documentation" (genvdp.txt) and
/// the Kabuto hardware notes (plutiedev.com/mirror/kabuto-hardware-notes).
///
/// Granularity note: on this emulator the offset is sampled at 68000
/// instruction boundaries (the CPU steps whole instructions), so the returned H
/// value advances in per-instruction steps rather than per-dot. That is
/// sufficient to represent the counter's monotonic rise and the mid-line jump
/// for HV-based RNG / raster timing.
fn hcounter_from_offset(offset_ticks: u64, h40: bool) -> u8 {
    let (steps, visible_max, jump_start): (u64, u64, u64) = if h40 {
        (HCOUNTER_STEPS_H40, 0xB6, 0xE4)
    } else {
        (HCOUNTER_STEPS_H32, 0x93, 0xE9)
    };
    // Clamp so an offset exactly at (or past) the line length maps to the final
    // dot (0xFF) rather than overflowing into the next line's range.
    let clamped = offset_ticks.min(MASTER_TICKS_PER_LINE_H40 - 1);
    // Linear map: dot index 0..steps across the whole line.
    let dot = (clamped * steps) / MASTER_TICKS_PER_LINE_H40; // 0..=steps-1
    if dot <= visible_max {
        dot as u8
    } else {
        // Past the jump: continue from `jump_start` up through 0xFF.
        (dot - visible_max - 1 + jump_start) as u8
    }
}

/// VRAM size in bytes.
pub const VRAM_SIZE: usize = 0x10000; // 64KB
/// Number of CRAM entries (9-bit RGB color values stored as u16).
pub const CRAM_ENTRIES: usize = 64; // 4 palettes x 16 colors
/// Number of VSRAM entries (vertical scroll values).
pub const VSRAM_ENTRIES: usize = 40;
/// Number of VDP registers.
pub const VDP_REGISTER_COUNT: usize = 24;

/// VDP access type set by the control port command words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessType {
    VramRead,
    VramWrite,
    CramRead,
    CramWrite,
    VsramRead,
    VsramWrite,
}

/// State of the control port command word parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlState {
    /// Waiting for the first command word.
    Idle,
    /// Received first word, waiting for second.
    PendingSecond(u16),
}

/// DMA mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DmaMode {
    /// 68K to VRAM/CRAM/VSRAM transfer.
    MemoryToVram,
    /// VRAM fill.
    Fill,
    /// VRAM to VRAM copy.
    Copy,
}

/// Serializable VDP snapshot.
///
/// Captures the complete deterministic VDP state (everything except the RGBA
/// framebuffer, which is re-derived by rendering). This is used both for save
/// states and for time-travel rewind, so it must be complete enough that
/// `Vdp::restore` reproduces bit-identical subsequent emulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VdpSnapshot {
    pub vram: Vec<u8>,
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
    /// Latched V-interrupt-pending (VIP) flag — see [`Vdp::vint_pending`].
    pub vint_pending: bool,
    /// Remaining CPU cycles for which the DMA-busy status bit stays asserted
    /// and the 68000 is held off the bus — see [`Vdp::dma_busy_cpu_cycles`].
    pub dma_busy_cpu_cycles: u32,
    /// Console region. Selects PAL V-counter wrap and enables V30 (240-line)
    /// mode. Defaults to NTSC for save states written before region support.
    #[serde(default)]
    pub region: Region,
}

/// Maximum sprites evaluated per frame (H40 mode).
const MAX_SPRITES_TOTAL_H40: usize = 80;
/// Maximum sprites per scanline in H40 mode.
const MAX_SPRITES_PER_LINE_H40: usize = 20;
/// Maximum sprites evaluated per frame (H32 mode).
const MAX_SPRITES_TOTAL_H32: usize = 64;
/// Maximum sprites per scanline in H32 mode.
const MAX_SPRITES_PER_LINE_H32: usize = 16;

// VDP register 0x0C (mode register 4) horizontal resolution select bits.
/// RS0 (bit 0) — horizontal resolution select, low bit.
const RS0: u8 = 0x01;
/// RS1 (bit 7) — horizontal resolution select, high bit.
const RS1: u8 = 0x80;

/// A single render-relevant state change recorded mid-scanline.
///
/// See [`LineEvent`]. `Register` carries the register index and its new 8-bit
/// value; `Cram`/`Vsram` carry the entry index and the already-masked value;
/// `Hscroll` carries the plane (0 = A, 1 = B) and the new horizontal-scroll word
/// (as written to the plane's HSCROLL slot in VRAM for this line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineChange {
    Register { reg: u8, value: u8 },
    Cram { index: u8, value: u16 },
    Vsram { index: u8, value: u16 },
    Hscroll { plane: u8, value: u16 },
}

/// A mid-line register/memory change recorded during the current scanline,
/// tagged with the beam dot (pixel column) at which it took effect.
///
/// Events are appended in ascending `dot` order because the 68000 executes
/// instructions in beam order across the line, so the recording path never
/// needs to sort. Transient per-line state — never serialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineEvent {
    dot: u16,
    change: LineChange,
}

/// Start-of-line snapshot of the render-relevant state, captured lazily on the
/// first recorded [`LineEvent`] of a scanline (before that write mutates the
/// live state). The span renderer replays from this baseline, applying each
/// event at its dot, so the pixels left of an event reflect the pre-write state
/// and the pixels right of it reflect the post-write state.
///
/// Transient per-line state — never serialized (not part of [`VdpSnapshot`]).
#[derive(Debug, Clone, Copy)]
struct LineBaseline {
    registers: [u8; VDP_REGISTER_COUNT],
    cram: [u16; CRAM_ENTRIES],
    vsram: [u16; VSRAM_ENTRIES],
    hscroll: (i16, i16),
}

/// The VDP.
pub struct Vdp {
    /// 64KB Video RAM.
    vram: [u8; VRAM_SIZE],
    /// Color RAM — 64 entries of 9-bit RGB (stored in lower 9 bits of u16).
    cram: [u16; CRAM_ENTRIES],
    /// Vertical scroll RAM — per-column vertical scroll offsets.
    vsram: [u16; VSRAM_ENTRIES],
    /// VDP registers (24 registers, 8-bit each).
    registers: [u8; VDP_REGISTER_COUNT],
    /// Control port state machine.
    control_state: ControlState,
    /// Current VRAM/CRAM/VSRAM address for data port access.
    address: u16,
    /// Auto-increment value (register 0x0F).
    auto_increment: u16,
    /// Current access type.
    access_type: Option<AccessType>,
    /// Current scanline (0-261 NTSC, 0-312 PAL).
    scanline: u16,
    /// Current dot within scanline.
    dot: u16,
    /// Console region (NTSC/PAL). Governs the V-counter wrap and whether V30
    /// (240-line) mode is available.
    region: Region,
    /// RGBA framebuffer. Physically sized to the maximum (H40 width × V30 height,
    /// 320×240) so neither an H32↔H40 width change nor a V28↔V30 height change
    /// ever reallocates. Only the active `frame_width × active_height()` prefix
    /// (packed at the latched width stride) is rendered and returned by
    /// [`Vdp::framebuffer`].
    framebuffer: Box<[u8; FRAME_RGBA_BYTES_MAX]>,
    /// Active display width (in pixels) latched for the current frame.
    ///
    /// Horizontal mode can change mid-frame if a game writes reg 0x0C between
    /// scanlines, but a single frame must use ONE consistent stride to stay
    /// coherent. This is captured once at the start of each frame (scanline 0 of
    /// `render_scanline`) and used as both the writeback stride and the returned
    /// framebuffer length for that whole frame; a mode change only takes effect
    /// at the next frame boundary. This is a per-frame derived cache recomputed
    /// every frame (and from the registers on `restore`), NOT persistent state,
    /// so it is intentionally excluded from `VdpSnapshot`.
    frame_width: u16,
    /// V-blank flag.
    in_vblank: bool,
    /// H-blank flag.
    in_hblank: bool,
    /// DMA pending flag — set when a control port write triggers DMA.
    dma_pending: bool,
    /// DMA fill pending — the next data port write supplies the fill value.
    dma_fill_pending: bool,
    /// H-interrupt counter (reloaded from register 0x0A each frame).
    h_interrupt_counter: i16,
    /// H-interrupt pending flag — set when counter expires and H-int is enabled.
    h_interrupt_pending: bool,
    /// Combined control code (CD5-CD0) from two-word command sequence.
    control_code: u8,
    /// Odd frame toggle — flipped each frame for interlace/status register.
    odd_frame: bool,
    /// Latched V-interrupt-pending (VIP) flag.
    ///
    /// The VDP raises this at the start of V-blank when the V-interrupt is
    /// enabled and holds it until the level-6 interrupt is actually taken by
    /// the 68000. Because the 68000 may be inside a mask-7 critical section at
    /// the instant V-blank begins, delivering the interrupt as a one-shot at
    /// that scanline would silently drop it (SGDK disables interrupts during
    /// boot for several frames). Latching it so it is taken as soon as the CPU
    /// mask falls below 6 matches the level-triggered IPL lines on hardware and
    /// is what lets SGDK's V-blank-driven tilemap/DMA path make progress.
    vint_pending: bool,
    /// Remaining CPU cycles for which a DMA holds the bus.
    ///
    /// While this is non-zero the DMA-busy status bit (bit 1) reads as set and
    /// the 68000 is stalled off the bus, matching hardware where a 68K→VRAM
    /// DMA / VRAM fill / VRAM copy freezes the CPU for the transfer's duration.
    /// It is decremented as CPU cycles elapse and reaches zero when the
    /// transfer completes.
    dma_busy_cpu_cycles: u32,

    // ---- Mid-line raster recording (transient per-line state) ----
    //
    // These three fields implement TRUE mid-line raster: a register/CRAM/VSRAM/
    // HSCROLL write that lands part-way through an active scanline splits the
    // line so the pixels left of the write use the old state and the pixels
    // right of it use the new state. They are reset every scanline in
    // `begin_scanline` and are deliberately NOT part of `VdpSnapshot` (like
    // `frame_width`, they are per-line derived state, so save/rewind stays
    // byte-identical).
    /// Mid-line changes recorded during the current line, in ascending dot order.
    /// Empty on the overwhelmingly common line (fast render path).
    line_events: Vec<LineEvent>,
    /// Start-of-line snapshot, captured lazily on the first recorded event of a
    /// line. `None` until (and unless) a mid-line event is recorded.
    line_baseline: Option<LineBaseline>,
    /// True while an active, display-enabled line is being executed, so timed
    /// CPU writes on that line get recorded as [`LineEvent`]s.
    recording_line: bool,
}

impl Vdp {
    /// Creates a new VDP in its power-on state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            vram: [0; VRAM_SIZE],
            cram: [0; CRAM_ENTRIES],
            vsram: [0; VSRAM_ENTRIES],
            registers: [0; VDP_REGISTER_COUNT],
            control_state: ControlState::Idle,
            address: 0,
            auto_increment: 0,
            access_type: None,
            scanline: 0,
            dot: 0,
            region: Region::Ntsc,
            framebuffer: Box::new([0; FRAME_RGBA_BYTES_MAX]),
            frame_width: FRAME_WIDTH as u16,
            in_vblank: false,
            in_hblank: false,
            dma_pending: false,
            dma_fill_pending: false,
            h_interrupt_counter: 0,
            h_interrupt_pending: false,
            control_code: 0,
            odd_frame: false,
            vint_pending: false,
            dma_busy_cpu_cycles: 0,
            line_events: Vec::new(),
            line_baseline: None,
            recording_line: false,
        }
    }

    /// Returns the length in bytes of the active framebuffer for the current
    /// frame: `frame_width * active_height() * 4`. The width is the latched
    /// native display width (320 in H40, 256 in H32); the height is 224 in V28
    /// or 240 in PAL V30. Both dimensions are packed at the latched width stride.
    #[must_use]
    fn framebuffer_len(&self) -> usize {
        self.frame_width as usize * self.active_height() * 4
    }

    /// Returns a reference to the active RGBA framebuffer for the current frame.
    ///
    /// The slice covers exactly the current active display area
    /// (`frame_width × active_height()`): 320×224 in H40/V28, 256×224 in H32/V28,
    /// or 320×240 in H40/PAL-V30. It is packed with a row stride equal to the
    /// latched active width (not the physical 320-wide allocation), and the
    /// backing buffer is always allocated at the 320×240 maximum, so only the
    /// active prefix is exposed.
    #[must_use]
    pub fn framebuffer(&self) -> &[u8] {
        &self.framebuffer[..self.framebuffer_len()]
    }

    /// Returns the width (in pixels) of the framebuffer returned by
    /// [`Vdp::framebuffer`] for the current frame.
    ///
    /// This is the latched per-frame width (320 in H40, 256 in H32), which
    /// matches the packed stride of the framebuffer slice. A mid-frame reg 0x0C
    /// write does not change it until the next frame boundary — so this is the
    /// value a frontend must size its output buffer to.
    #[must_use]
    pub fn display_width(&self) -> u16 {
        self.frame_width
    }

    /// Console region (NTSC/PAL).
    #[must_use]
    pub fn region(&self) -> Region {
        self.region
    }

    /// True when configured for a 50 Hz PAL region.
    #[must_use]
    pub fn is_pal(&self) -> bool {
        self.region.is_pal()
    }

    /// Sets the console region. Governs the V-counter wrap and V30 availability.
    pub fn set_region(&mut self, region: Region) {
        self.region = region;
    }

    /// Active display height in scanlines, derived purely from the region and
    /// reg 0x01 bit 3 (M2 / V30 select): 240 lines in PAL V30 mode, otherwise
    /// 224 (V28). V30 is only valid on PAL hardware; on NTSC the bit is ignored.
    #[must_use]
    pub fn active_height(&self) -> usize {
        if self.is_pal() && (self.registers[0x01] & 0x08 != 0) {
            ACTIVE_HEIGHT_V30
        } else {
            ACTIVE_HEIGHT_V28
        }
    }

    /// Returns the current scanline.
    #[must_use]
    pub fn scanline(&self) -> u16 {
        self.scanline
    }

    /// Returns true if in vertical blanking interval.
    #[must_use]
    pub fn in_vblank(&self) -> bool {
        self.in_vblank
    }

    /// Returns true if an H-interrupt is pending delivery to the CPU.
    #[must_use]
    pub fn h_interrupt_pending(&self) -> bool {
        self.h_interrupt_pending
    }

    /// Clears the pending H-interrupt flag after the CPU acknowledges it.
    pub fn clear_h_interrupt(&mut self) {
        self.h_interrupt_pending = false;
    }

    /// Writes to the VDP control port.
    ///
    /// Handles the two-word command sequence and register writes.
    ///
    /// `dot` is the intra-line beam position (pixel column) of a timed live-CPU
    /// write: `Some(x)` records a mid-line register change when the current line
    /// is being recorded (see [`Vdp::record_dot`]); `None` is an untimed write
    /// (DMA, debug, interrupt-handler / between-lines delivery) that applies
    /// globally and records nothing.
    pub fn write_control(&mut self, value: u16, dot: Option<u16>) {
        match self.control_state {
            ControlState::Idle => {
                // Check if this is a register write: 100x xxxx xxxx xxxx
                if value & 0xC000 == 0x8000 {
                    let reg = ((value >> 8) & 0x1F) as usize;
                    let data = (value & 0xFF) as u8;
                    if reg < VDP_REGISTER_COUNT {
                        // LAZY BASELINE: `self.registers` still holds the
                        // start-of-line value for this register (this write has
                        // not applied yet), so snapshot the baseline BEFORE
                        // mutating, then record the change at its dot.
                        if let Some(x) = self.record_dot(dot) {
                            self.record_change(
                                x,
                                LineChange::Register {
                                    reg: reg as u8,
                                    value: data,
                                },
                            );
                        }
                        self.registers[reg] = data;
                        if reg == 0x0F {
                            self.auto_increment = u16::from(data);
                        }
                    }
                    // Register writes don't change control state
                } else {
                    // First word of a two-word command
                    self.control_state = ControlState::PendingSecond(value);
                }
            }
            ControlState::PendingSecond(first) => {
                // Combine first and second words to get access type + address
                let cd = ((first >> 14) & 0x03) | ((value >> 2) & 0x3C);
                let addr = (first & 0x3FFF) | ((value & 0x03) << 14);

                self.address = addr;
                self.control_code = cd as u8;
                self.access_type = match cd & 0x0F {
                    0b0000 => Some(AccessType::VramRead),
                    0b0001 => Some(AccessType::VramWrite),
                    0b1000 => Some(AccessType::CramRead),
                    0b0011 => Some(AccessType::CramWrite),
                    0b0100 => Some(AccessType::VsramRead),
                    0b0101 => Some(AccessType::VsramWrite),
                    _ => None,
                };

                // Check for DMA trigger: CD5 set and DMA enabled (reg 1 bit 4)
                if cd & 0x20 != 0 && self.registers[1] & 0x10 != 0 {
                    let dma_mode = self.registers[0x17] >> 6;
                    match dma_mode {
                        0b00 | 0b01 => {
                            // 68K-to-VRAM DMA — will be executed by caller
                            self.dma_pending = true;
                        }
                        0b10 => {
                            // VRAM fill — triggered by next data port write
                            self.dma_fill_pending = true;
                        }
                        0b11 => {
                            // VRAM copy — not yet implemented
                        }
                        _ => {}
                    }
                }

                self.control_state = ControlState::Idle;
            }
        }
    }

    /// Writes to the VDP data port.
    ///
    /// `dot` has the same meaning as in [`Vdp::write_control`]: `Some(x)` is a
    /// timed live-CPU write whose CRAM / VSRAM / (per-line HSCROLL-slot) VRAM
    /// change is recorded as a mid-line event; `None` applies globally without
    /// recording.
    pub fn write_data(&mut self, value: u16, dot: Option<u16>) {
        // Reset control state on data port access
        self.control_state = ControlState::Idle;

        // Check if this data write triggers a VRAM fill DMA
        if self.dma_fill_pending {
            self.dma_fill_pending = false;
            self.execute_dma_fill(value);
            return;
        }

        let record = self.record_dot(dot);
        match self.access_type {
            Some(AccessType::VramWrite) => {
                let addr = self.address as usize;
                // A VRAM write only produces a mid-line split if it lands on
                // this line's HSCROLL slot (plane A / B); ordinary nametable /
                // tile-data VRAM writes are applied to `self.vram` immediately
                // and thus already visible to the whole line's plane fetch, as
                // before (out of scope for the span model).
                if let Some(x) = record
                    && let Some(plane) = self.hscroll_slot_plane(self.scanline, self.address)
                {
                    self.record_change(x, LineChange::Hscroll { plane, value });
                }
                if addr < VRAM_SIZE - 1 {
                    self.vram[addr] = (value >> 8) as u8;
                    self.vram[addr + 1] = value as u8;
                }
            }
            Some(AccessType::CramWrite) => {
                let index = (self.address >> 1) as usize;
                let masked = value & 0x0EEE; // 9-bit: 0BBB0GGG0RRR
                if index < CRAM_ENTRIES {
                    if let Some(x) = record {
                        self.record_change(
                            x,
                            LineChange::Cram {
                                index: (index & 0x3F) as u8,
                                value: masked,
                            },
                        );
                    }
                    self.cram[index] = masked;
                }
            }
            Some(AccessType::VsramWrite) => {
                let index = (self.address >> 1) as usize;
                let masked = value & 0x07FF; // 11-bit scroll value
                if index < VSRAM_ENTRIES {
                    if let Some(x) = record {
                        self.record_change(
                            x,
                            LineChange::Vsram {
                                index: index as u8,
                                value: masked,
                            },
                        );
                    }
                    self.vsram[index] = masked;
                }
            }
            _ => {}
        }
        self.address = self.address.wrapping_add(self.auto_increment);
    }

    /// Returns the recordable beam dot for a timed write, or `None` when the
    /// write must not be recorded.
    ///
    /// A change is recorded only when (a) the write is timed (`dot` is `Some`),
    /// (b) the current line is actively recording (an active, display-enabled
    /// line — see [`Vdp::begin_scanline`]), and (c) the dot lies strictly inside
    /// the active display (`1..width`). Dot 0 (and the line edges) are excluded:
    /// a write there is indistinguishable from a start-of-line / hblank write
    /// and applying it as the baseline is already correct.
    #[inline]
    fn record_dot(&self, dot: Option<u16>) -> Option<u16> {
        let x = dot?;
        if self.recording_line && x >= 1 && x < self.screen_width() {
            Some(x)
        } else {
            None
        }
    }

    /// Records a mid-line change at beam dot `x`: captures the start-of-line
    /// baseline (once per line) and appends the event. Must be called BEFORE the
    /// triggering write mutates the live state, so the lazy baseline snapshots the
    /// true start-of-line values (see [`Vdp::capture_line_baseline`]).
    fn record_change(&mut self, x: u16, change: LineChange) {
        self.capture_line_baseline();
        self.line_events.push(LineEvent { dot: x, change });
    }

    /// Captures the start-of-line baseline on the first recorded event of a line.
    ///
    /// Must be called BEFORE the triggering write mutates `self.registers` /
    /// `self.cram` / `self.vsram` / the HSCROLL VRAM slot, so the snapshot holds
    /// the true start-of-line values. Idempotent within a line.
    fn capture_line_baseline(&mut self) {
        if self.line_baseline.is_none() {
            self.line_baseline = Some(LineBaseline {
                registers: self.registers,
                cram: self.cram,
                vsram: self.vsram,
                hscroll: self.hscroll_for_line(self.scanline),
            });
        }
    }

    /// If `addr` is the VRAM word address of this `line`'s HSCROLL slot, returns
    /// the plane it belongs to (0 = A, 1 = B); otherwise `None`.
    ///
    /// Mirrors the addressing in [`Vdp::hscroll_for_line`]: base from reg 0x0D,
    /// offset by the reg 0x0B H-scroll mode (full / per-cell / per-line). Plane A
    /// occupies the slot word, plane B the following word.
    #[must_use]
    fn hscroll_slot_plane(&self, line: u16, addr: u16) -> Option<u8> {
        let base = self.hscroll_table_addr();
        let mode = self.registers[0x0B] & 0x03;
        let offset = match mode {
            0b00 => 0,
            0b10 => (line as usize / 8) * 4,
            0b11 => line as usize * 4,
            _ => 0,
        };
        let slot = (base + offset) & (VRAM_SIZE - 1);
        let a = addr as usize;
        if a == slot {
            Some(0)
        } else if a == (slot + 2) & (VRAM_SIZE - 1) {
            Some(1)
        } else {
            None
        }
    }

    /// Maps an intra-line master-tick offset to a beam dot (pixel column).
    ///
    /// Linear map `((offset.min(line-1) * width) / line)` where `width` is the
    /// current horizontal resolution (320 in H40, 256 in H32) and `line` is
    /// [`MASTER_TICKS_PER_LINE_H40`]. This is a deliberate approximation that
    /// ignores the ~13% horizontal-blank porch: it is monotonic and
    /// self-consistent (the same offset always maps to the same dot), which is
    /// what the span renderer needs. Exact dot-accurate positioning — including
    /// the HCounter's mid-line jump over hblank, the accurate model used by
    /// [`Vdp::read_hv_counter`] / `hcounter_from_offset` — is a documented
    /// refinement, not required for the mid-line split mechanism.
    #[must_use]
    pub fn dot_from_line_offset(&self, offset: u64) -> u16 {
        let width = u64::from(self.screen_width());
        ((offset.min(MASTER_TICKS_PER_LINE_H40 - 1) * width) / MASTER_TICKS_PER_LINE_H40) as u16
    }

    /// Reads from the VDP data port.
    #[must_use]
    pub fn read_data(&mut self) -> u16 {
        self.control_state = ControlState::Idle;

        let result = match self.access_type {
            Some(AccessType::VramRead) => {
                let addr = self.address as usize;
                if addr < VRAM_SIZE - 1 {
                    u16::from(self.vram[addr]) << 8 | u16::from(self.vram[addr + 1])
                } else {
                    0
                }
            }
            Some(AccessType::CramRead) => {
                let index = (self.address >> 1) as usize;
                if index < CRAM_ENTRIES {
                    self.cram[index]
                } else {
                    0
                }
            }
            Some(AccessType::VsramRead) => {
                let index = (self.address >> 1) as usize;
                if index < VSRAM_ENTRIES {
                    self.vsram[index]
                } else {
                    0
                }
            }
            _ => 0,
        };
        self.address = self.address.wrapping_add(self.auto_increment);
        result
    }

    /// Reads a VDP register value.
    #[must_use]
    pub fn read_register(&self, reg: usize) -> u8 {
        if reg < VDP_REGISTER_COUNT {
            self.registers[reg]
        } else {
            0
        }
    }

    /// Reads the VDP status register.
    #[must_use]
    pub fn read_status(&self) -> u16 {
        let mut status: u16 = 0x3400; // Always set bits
        // Bit 9: FIFO empty (always set — no FIFO emulation)
        status |= 0x0200;
        // Bit 3: V-blank
        if self.in_vblank {
            status |= 0x0008;
        }
        // Bit 4: Odd frame (toggles each frame)
        if self.odd_frame {
            status |= 0x0010;
        }
        // Bit 2: H-blank
        if self.in_hblank {
            status |= 0x0004;
        }
        // Bit 1: DMA busy — asserted for the whole duration of an in-flight
        // transfer (68K→VRAM/CRAM/VSRAM, VRAM fill, or VRAM copy), cleared once
        // the transfer's cycle budget has elapsed.
        if self.dma_busy_cpu_cycles > 0 {
            status |= 0x0002;
        }
        // Bit 7: V-interrupt pending (VIP) — latched at V-blank, cleared when
        // the level-6 interrupt is taken.
        if self.vint_pending {
            status |= 0x0080;
        }
        status
    }

    /// Returns the current HV counter value for a given intra-line beam position.
    ///
    /// High byte = V counter (scanline number, with the NTSC mid-frame jump).
    /// Low byte = H counter (horizontal beam position, with the mid-line jump).
    ///
    /// `line_offset_ticks` is the beam's position within the current scanline,
    /// in master-clock ticks measured from the line's start
    /// (`0..MASTER_TICKS_PER_LINE_H40`). The caller derives it from the
    /// scheduler/master-tick timeline at the instant of the read, giving a real
    /// (per-instruction-granular) intra-line H value rather than a two-valued
    /// hblank stub.
    ///
    /// # HV counter tables (source)
    ///
    /// - H40: HCounter `0x00..=0xB6` then `0xE4..=0xFF`.
    /// - H32: HCounter `0x00..=0x93` then `0xE9..=0xFF`.
    /// - NTSC V (262-line): VCounter `0x00..=0xEA` then `0xE5..=0xFF`.
    /// - PAL V (313-line): VCounter `0x00..=0x102` then `0xCA..=0xFF` (the V
    ///   counter is region-aware, from PR #12's region model).
    ///
    /// From Charles MacDonald "Sega Genesis VDP documentation" (genvdp.txt) and
    /// the Kabuto hardware notes (plutiedev.com/mirror/kabuto-hardware-notes).
    #[must_use]
    pub fn read_hv_counter(&self, line_offset_ticks: u64) -> u16 {
        // The 8-bit V counter cannot represent every line of a >256-line frame,
        // so hardware repeats a value range mid-frame. The wrap point differs by
        // region.
        let v = if self.region.is_pal() {
            // PAL (313 lines): counts 0x00-0xFF, 0x00-0x02, then jumps back to
            // 0xCA-0xFF. That is 256 + 3 + 54 = 313 distinct lines.
            if self.scanline <= 0x102 {
                self.scanline as u8
            } else {
                // After line 0x102 the counter resumes at 0xCA: line 0x103 → 0xCA
                // and line 0x138 (312) → 0xFF, i.e. subtract 0x39.
                (self.scanline.wrapping_sub(0x39)) as u8
            }
        } else {
            // NTSC (262 lines): 0x00-0xEA for lines 0-234, then jumps to 0xE5.
            if self.scanline <= 0xEA {
                self.scanline as u8
            } else {
                (self.scanline.wrapping_sub(6)) as u8
            }
        };

        // H counter: real intra-line beam position with the documented mid-line
        // jump, chosen by the current H40/H32 mode. Use `is_h40()` — the single
        // source of truth for horizontal resolution (reg 0x0C RS0|RS1) — so the
        // HCounter mode selection matches the same width detection that drives
        // per-frame width latching and rendering.
        let h = hcounter_from_offset(line_offset_ticks, self.is_h40());

        (u16::from(v) << 8) | u16::from(h)
    }

    /// Converts a 9-bit Genesis color (0BBB0GGG0RRR) to RGBA.
    #[must_use]
    fn color_to_rgba(color: u16) -> [u8; 4] {
        let r = ((color & 0x00E) >> 1) as u8;
        let g = ((color & 0x0E0) >> 5) as u8;
        let b = ((color & 0xE00) >> 9) as u8;
        // Scale 3-bit (0-7) to 8-bit (0-255)
        [r * 36 + r / 2, g * 36 + g / 2, b * 36 + b / 2, 0xFF]
    }

    /// Applies a shadow/highlight intensity to an already-Normal RGBA pixel.
    ///
    /// Because Shadow = Normal/2 and Highlight = 128 + Normal/2 are both linear
    /// in the 8-bit Normal value, the transform is applied directly to the
    /// stored RGBA rather than re-derived from CRAM. Only RGB is modified; the
    /// alpha channel is preserved. `intensity`: 0 = Shadow, 1 = Normal,
    /// 2 = Highlight.
    #[must_use]
    fn apply_intensity(rgba: [u8; 4], intensity: u8) -> [u8; 4] {
        match intensity {
            0 => [rgba[0] >> 1, rgba[1] >> 1, rgba[2] >> 1, rgba[3]], // shadow
            2 => [
                128 + (rgba[0] >> 1),
                128 + (rgba[1] >> 1),
                128 + (rgba[2] >> 1),
                rgba[3],
            ], // highlight
            _ => rgba, // normal
        }
    }

    /// Returns the background color (palette 0, color 0).
    #[must_use]
    pub fn background_color(&self) -> [u8; 4] {
        let bg_index = (self.registers[7] & 0x3F) as usize;
        if bg_index < CRAM_ENTRIES {
            Self::color_to_rgba(self.cram[bg_index])
        } else {
            [0, 0, 0, 0xFF]
        }
    }

    // ---- DMA ----

    /// Returns true if a 68K-to-VRAM DMA is pending.
    #[must_use]
    pub fn dma_pending(&self) -> bool {
        self.dma_pending
    }

    /// Sets the V-blank flag. Called by the core at the appropriate scanline.
    pub fn set_vblank(&mut self, val: bool) {
        self.in_vblank = val;
    }

    /// Returns true while a DMA holds the bus (DMA-busy status bit is set).
    #[must_use]
    pub fn dma_busy(&self) -> bool {
        self.dma_busy_cpu_cycles > 0
    }

    /// Returns the number of CPU cycles for which the current DMA still holds
    /// the bus. Zero when no DMA is in flight.
    #[must_use]
    pub fn dma_busy_cpu_cycles(&self) -> u32 {
        self.dma_busy_cpu_cycles
    }

    /// Advances DMA-busy accounting by `cycles` CPU cycles, clearing the
    /// DMA-busy state once the transfer's budget has fully elapsed. Called by
    /// the core as the 68000 (or a stall) consumes cycles.
    pub fn advance_dma_busy(&mut self, cycles: u32) {
        self.dma_busy_cpu_cycles = self.dma_busy_cpu_cycles.saturating_sub(cycles);
    }

    /// Returns true if a V-interrupt (VIP) is latched and awaiting delivery.
    #[must_use]
    pub fn vint_pending(&self) -> bool {
        self.vint_pending
    }

    /// Latches the V-interrupt-pending (VIP) flag. Called by the core at the
    /// start of V-blank when the V-interrupt is enabled.
    pub fn set_vint_pending(&mut self) {
        self.vint_pending = true;
    }

    /// Clears the latched V-interrupt-pending flag. Called once the level-6
    /// interrupt has actually been taken by the 68000.
    pub fn clear_vint_pending(&mut self) {
        self.vint_pending = false;
    }

    /// CPU cycles the CPU is stalled per scanline of active display.
    ///
    /// One NTSC scanline is 3420 master clocks; at 1 CPU cycle ≈ 7 master
    /// clocks that is ≈ 488 CPU cycles, matching the core's per-scanline budget.
    ///
    /// This is identical for H32 and H40: both consume 3420 master clocks per
    /// scanline (hence the same 488 CPU cycles/line). They differ only in the
    /// dot clock — H32 draws 256 dots at the slower EDCLK-derived pixel clock,
    /// H40 draws 320 dots at the faster clock — and in the DMA byte-per-line
    /// budget, which `dma_cost_cycles`'s `bytes_per_line` table accounts for
    /// via its `(h40, blanking)` key.
    ///
    /// Centralized in [`crate::timing`]; used here purely as the coarse per-line
    /// DMA word-transfer budget (the Software Manual quotes DMA slot rates per
    /// scanline).
    const CPU_CYCLES_PER_LINE: u32 = crate::timing::CPU_CYCLES_PER_LINE_H40;

    /// Computes the CPU-cycle cost (and hence DMA-busy duration / CPU stall) of
    /// a DMA transfer of `length` words.
    ///
    /// The Sega Genesis Software Manual specifies how many bytes a DMA can move
    /// per scanline; the figures differ between active display and blanking and
    /// between H32 and H40. In H40 a 68K→VRAM DMA moves ~205 bytes/line during
    /// blanking versus ~18 bytes/line during active display; in H32 it is ~167
    /// vs ~16 bytes/line (see plutiedev.com "DMA transfers" and Nemesis's timing
    /// research on SpritesMind, which tabulate the same slot rates). CRAM/VSRAM
    /// writes and VRAM fills move one word per slot like VRAM writes; a VRAM→VRAM
    /// copy needs a read *and* a write per unit and so runs at half the rate.
    ///
    /// A VRAM word occupies two bytes, so words/line = bytes/line ÷ 2. The cost
    /// is the fraction of scanlines the transfer occupies, rounded up, times the
    /// per-line CPU-cycle budget.
    #[must_use]
    fn dma_cost_cycles(&self, length: u32, copy: bool) -> u32 {
        if length == 0 {
            return 0;
        }
        let h40 = self.is_h40();
        let blanking = self.in_vblank || (self.registers[1] & 0x40 == 0);
        // Bytes movable per scanline, per the Software Manual DMA timing table.
        let bytes_per_line: u32 = match (h40, blanking) {
            (true, true) => 205,
            (true, false) => 18,
            (false, true) => 167,
            (false, false) => 16,
        };
        // Words per line (each VRAM word = 2 bytes); copy runs at half rate.
        let mut words_per_line = (bytes_per_line / 2).max(1);
        if copy {
            words_per_line = (words_per_line / 2).max(1);
        }
        // Fraction of scanlines occupied, rounded up, times the per-line budget.
        let lines = length.div_ceil(words_per_line);
        lines.saturating_mul(Self::CPU_CYCLES_PER_LINE)
    }

    /// Executes a pending 68K-to-VRAM/CRAM/VSRAM DMA transfer.
    ///
    /// `read_word` is a callback that reads a 16-bit word from the 68K address space.
    /// The DMA source address is assembled from registers 0x15-0x17 and shifted left
    /// by 1 to form a byte address. Length comes from registers 0x13-0x14 (in words).
    pub fn run_dma(&mut self, read_word: &mut dyn FnMut(u32) -> u16) {
        if !self.dma_pending {
            return;
        }
        self.dma_pending = false;

        let length = u32::from(self.registers[0x13]) | (u32::from(self.registers[0x14]) << 8);
        if length == 0 {
            return;
        }

        // Hold the bus (assert DMA-busy and stall the 68000) for the transfer's
        // cycle cost. The data is moved in one shot below, but the busy window
        // and CPU stall are charged so software that overlaps DMA with CPU work,
        // polls DMA-busy, or spreads a transfer across V-blank stays in sync.
        self.dma_busy_cpu_cycles = self.dma_cost_cycles(length, false);

        let src_base = u32::from(self.registers[0x15])
            | (u32::from(self.registers[0x16]) << 8)
            | (u32::from(self.registers[0x17] & 0x7F) << 16);
        // Source is a word address; shift left by 1 for byte address
        let mut src_addr = src_base << 1;

        for _ in 0..length {
            let word = read_word(src_addr);
            match self.access_type {
                Some(AccessType::VramWrite) => {
                    let addr = self.address as usize;
                    if addr < VRAM_SIZE - 1 {
                        self.vram[addr] = (word >> 8) as u8;
                        self.vram[addr + 1] = word as u8;
                    }
                }
                Some(AccessType::CramWrite) => {
                    let index = (self.address >> 1) as usize;
                    if index < CRAM_ENTRIES {
                        self.cram[index] = word & 0x0EEE;
                    }
                }
                Some(AccessType::VsramWrite) => {
                    let index = (self.address >> 1) as usize;
                    if index < VSRAM_ENTRIES {
                        self.vsram[index] = word & 0x07FF;
                    }
                }
                _ => {}
            }
            self.address = self.address.wrapping_add(self.auto_increment);
            src_addr = src_addr.wrapping_add(2);
        }

        // Update DMA source registers to reflect final position
        let final_src = src_addr >> 1;
        self.registers[0x15] = final_src as u8;
        self.registers[0x16] = (final_src >> 8) as u8;
        self.registers[0x17] = (self.registers[0x17] & 0x80) | ((final_src >> 16) as u8 & 0x7F);
        // Zero the length registers
        self.registers[0x13] = 0;
        self.registers[0x14] = 0;
    }

    /// Executes a VRAM fill DMA. The fill value's HIGH byte is written to
    /// successive VRAM addresses.
    fn execute_dma_fill(&mut self, value: u16) {
        let length = u32::from(self.registers[0x13]) | (u32::from(self.registers[0x14]) << 8);
        let fill_byte = (value >> 8) as u8;

        // Hold the bus for the fill's duration (see `run_dma`).
        self.dma_busy_cpu_cycles = self.dma_cost_cycles(length.max(1), false);

        // First, write the full word to the current VRAM address
        let addr = self.address as usize;
        if addr < VRAM_SIZE - 1 {
            self.vram[addr] = (value >> 8) as u8;
            self.vram[addr + 1] = value as u8;
        }
        self.address = self.address.wrapping_add(self.auto_increment);

        // Then fill remaining length-1 locations with the high byte
        for _ in 1..length {
            let a = self.address as usize;
            if a < VRAM_SIZE {
                self.vram[a] = fill_byte;
            }
            self.address = self.address.wrapping_add(self.auto_increment);
        }

        self.registers[0x13] = 0;
        self.registers[0x14] = 0;
    }

    // ---- Scanline timing ----

    /// Called at the start of each scanline. Sets H-blank flag and manages
    /// the H-interrupt counter.
    pub fn begin_scanline(&mut self, line: u16) {
        self.scanline = line;
        self.in_hblank = true;

        // Reset the mid-line raster recording for this line. `Vec::clear`
        // retains capacity, so an active line that records events does not
        // allocate after the first frame. Recording is enabled only on an
        // active, display-enabled line — the only lines `render_scanline`
        // actually composites from planes; blanked / display-off lines just
        // fill the backdrop and never consult the events. This runs at the
        // very start of the line, BEFORE that line's CPU budget, so the
        // lazily-captured baseline sees untouched start-of-line state.
        self.line_events.clear();
        self.line_baseline = None;
        self.recording_line = (line as usize) < self.active_height() && self.display_enabled();

        // H-interrupt counter (reg 0x0A) behaviour, per the Genesis Software
        // Manual / Charles MacDonald VDP notes: the counter is reloaded from
        // reg 0x0A at the top of the frame (line 0) AND on every VBlank line.
        // On each active display line it decrements; when it underflows past
        // zero it reloads and, if H-interrupts are enabled (reg 0 bit 4),
        // latches an H-interrupt.
        //
        // The active/blanking boundary is region-aware via `active_height()`
        // (224 in NTSC/V28, 240 in PAL V30, from PR #12's region model), so the
        // VBlank reload window is `line >= active_height()` — the correct region's
        // vertical blanking. Reloading throughout VBlank (not only at line 0) is
        // what makes the first HINT of the next frame land on the correct line:
        // the counter always re-arms to its programmed period before active
        // display resumes.
        let active_height = self.active_height() as u16;
        if line == 0 || line >= active_height {
            self.h_interrupt_counter = i16::from(self.registers[0x0A]);
        } else {
            // Active scanlines (1..active_height): decrement counter.
            self.h_interrupt_counter -= 1;
            if self.h_interrupt_counter < 0 {
                // Counter expired — reload and fire interrupt if enabled.
                self.h_interrupt_counter = i16::from(self.registers[0x0A]);
                if self.registers[0] & 0x10 != 0 {
                    self.h_interrupt_pending = true;
                }
            }
        }
    }

    /// Called at the end of a visible frame. Resets scanline counter.
    pub fn end_frame(&mut self) {
        self.scanline = 0;
        self.in_hblank = false;
        self.odd_frame = !self.odd_frame;
    }

    // ---- Rendering ----

    /// Returns true when the VDP is in H40 (40-cell / 320-pixel) horizontal
    /// mode. This is the single source of truth for horizontal resolution.
    ///
    /// H40 requires BOTH RS0 (reg 0x0C bit 0) and RS1 (bit 7) to be set;
    /// every other bit combination selects H32 (32-cell / 256-pixel) mode.
    /// Real software uses 0x81 for H40 and 0x00 for H32, so the "both bits"
    /// rule matches hardware while rejecting the ambiguous single-bit cases.
    ///
    /// Vertical timing (NTSC vs PAL line count) is owned separately; this
    /// helper is strictly about horizontal width.
    #[inline]
    #[must_use]
    fn is_h40(&self) -> bool {
        (self.registers[0x0C] & (RS0 | RS1)) == (RS0 | RS1)
    }

    /// Returns the horizontal screen width based on the current mode.
    /// H40 = 320 pixels, H32 = 256 pixels.
    #[must_use]
    fn screen_width(&self) -> u16 {
        if self.is_h40() { 320 } else { 256 }
    }

    /// Returns true if the display is enabled (register 1, bit 6).
    #[must_use]
    fn display_enabled(&self) -> bool {
        self.registers[1] & 0x40 != 0
    }

    /// Reads a big-endian u16 word from VRAM at the given byte address.
    #[must_use]
    fn vram_read_word(&self, addr: usize) -> u16 {
        let a = addr & (VRAM_SIZE - 1);
        let a1 = (a + 1) & (VRAM_SIZE - 1);
        u16::from(self.vram[a]) << 8 | u16::from(self.vram[a1])
    }

    /// Decode scroll plane size from register 0x10.
    /// Returns (h_cells, v_cells).
    #[must_use]
    fn scroll_size(&self) -> (u16, u16) {
        let reg = self.registers[0x10];
        let h = match reg & 0x03 {
            0b00 => 32,
            0b01 => 64,
            0b11 => 128,
            _ => 32, // 0b10 is invalid, treat as 32
        };
        let v = match (reg >> 4) & 0x03 {
            0b00 => 32,
            0b01 => 64,
            0b11 => 128,
            _ => 32,
        };
        (h, v)
    }

    /// Returns the nametable base address for Scroll A.
    #[must_use]
    fn scroll_a_nametable_addr(&self) -> usize {
        usize::from(self.registers[0x02] & 0x38) << 10
    }

    /// Returns the nametable base address for Scroll B.
    #[must_use]
    fn scroll_b_nametable_addr(&self) -> usize {
        usize::from(self.registers[0x04] & 0x07) << 13
    }

    /// Returns the nametable base address for the window plane.
    #[must_use]
    fn window_nametable_addr(&self) -> usize {
        usize::from(self.registers[0x03] & 0x3E) << 10
    }

    /// Returns the horizontal pixel range where the window plane is active.
    /// Returns (left_pixel, right_pixel).
    #[must_use]
    fn window_h_range(&self, screen_width: u16) -> (u16, u16) {
        let reg = self.registers[0x11];
        let cells = u16::from(reg & 0x1F);
        // WHP (reg 0x11) is in units of 2 cells = 16 px on hardware.
        let pixels = cells * 16;
        if reg & 0x80 != 0 {
            // Window on the right side
            (pixels.min(screen_width), screen_width)
        } else {
            // Window on the left side
            (0, pixels.min(screen_width))
        }
    }

    /// Returns the vertical scanline range where the window plane is active.
    /// Returns (top_line, bottom_line).
    #[must_use]
    fn window_v_range(&self) -> (u16, u16) {
        let reg = self.registers[0x12];
        let cells = u16::from(reg & 0x1F);
        let lines = cells * 8;
        if reg & 0x80 != 0 {
            // Window below the split line
            (lines, self.active_height() as u16)
        } else {
            // Window above the split line
            (0, lines)
        }
    }

    /// Returns the nametable width in cells for the window plane.
    /// H40 mode = 64 cells wide, H32 mode = 32 cells wide.
    #[must_use]
    fn window_nametable_width(&self) -> u16 {
        if self.is_h40() { 64 } else { 32 }
    }

    /// Returns the H-scroll data table base address.
    #[must_use]
    fn hscroll_table_addr(&self) -> usize {
        usize::from(self.registers[0x0D] & 0x3F) << 10
    }

    /// Returns the sprite attribute table base address.
    #[must_use]
    fn sprite_table_addr(&self) -> usize {
        usize::from(self.registers[0x05] & 0x7F) << 9
    }

    /// Get H-scroll values for a given scanline.
    /// Returns (scroll_a, scroll_b).
    #[must_use]
    fn hscroll_for_line(&self, line: u16) -> (i16, i16) {
        let base = self.hscroll_table_addr();
        let mode = self.registers[0x0B] & 0x03;
        let offset = match mode {
            0b00 => 0,                       // Full screen
            0b10 => (line as usize / 8) * 4, // Per-cell (8 lines)
            0b11 => line as usize * 4,       // Per-line
            _ => 0,                          // 0b01 invalid, treat as full
        };
        let addr = base + offset;
        let scroll_a = self.vram_read_word(addr) as i16;
        let scroll_b = self.vram_read_word(addr + 2) as i16;
        (scroll_a, scroll_b)
    }

    /// Get V-scroll value for a given column and plane.
    /// `plane`: 0 = Scroll A, 1 = Scroll B.
    ///
    /// # Hardware behaviour
    ///
    /// Register 0x0B bit 2 selects the vertical-scroll mode:
    /// - **Full-plane** (bit clear): a single VSRAM value scrolls the whole
    ///   plane — `vsram[0]` for Scroll A, `vsram[1]` for Scroll B. Every column
    ///   uses the same value.
    /// - **2-cell-column** (bit set): VSRAM holds an independent vertical scroll
    ///   for each 16-pixel (2-cell) column, stored as interleaved A/B pairs —
    ///   `vsram[2*c]` = Scroll A, `vsram[2*c+1]` = Scroll B for column pair `c`.
    ///
    /// VSCROLL is consumed per-column as the beam advances left-to-right across
    /// the line, so a VSRAM write made part-way through the active line only
    /// affects the 2-cell columns the beam has not yet rendered. The span-based
    /// renderer models this: a recorded mid-line VSRAM change is applied at its
    /// beam dot, so columns to its right pick up the new value while columns to
    /// its left keep the old one.
    ///
    /// Sources: Charles MacDonald, "Sega Genesis VDP documentation" (vdp.txt),
    /// and the plutiedev.com VDP scrolling documentation.
    #[must_use]
    fn vscroll_for_column(&self, col: u16, plane: u8) -> u16 {
        let per_2cell = self.registers[0x0B] & 0x04 != 0;
        if per_2cell {
            // Per-2-cell mode: VSRAM index = column / 2 * 2 + plane
            let index = ((col / 2) * 2 + u16::from(plane)) as usize;
            if index < VSRAM_ENTRIES {
                self.vsram[index]
            } else {
                0
            }
        } else {
            // Full screen mode
            self.vsram[plane as usize]
        }
    }

    /// Fetches a single pixel from a tile pattern in VRAM.
    ///
    /// `tile_index`: pattern number (each tile is 32 bytes).
    /// `row`: pixel row within tile (0-7).
    /// `col`: pixel column within tile (0-7).
    /// `hflip`, `vflip`: mirror flags.
    ///
    /// Returns the 4-bit palette index (0 = transparent).
    #[must_use]
    fn tile_pixel(&self, tile_index: u16, row: u8, col: u8, hflip: bool, vflip: bool) -> u8 {
        let actual_row = if vflip { 7 - row } else { row };
        let actual_col = if hflip { 7 - col } else { col };

        // Each tile = 32 bytes, each row = 4 bytes
        let tile_addr = usize::from(tile_index) * 32 + usize::from(actual_row) * 4;
        let byte_offset = usize::from(actual_col / 2);
        let addr = (tile_addr + byte_offset) & (VRAM_SIZE - 1);
        let byte = self.vram[addr];

        if actual_col & 1 == 0 {
            (byte >> 4) & 0x0F // High nibble = left pixel
        } else {
            byte & 0x0F // Low nibble = right pixel
        }
    }

    /// Resolve a CRAM color from palette + index.
    #[must_use]
    fn resolve_color(&self, palette: u8, color_index: u8) -> [u8; 4] {
        let cram_index = (usize::from(palette) * 16 + usize::from(color_index)) & 0x3F;
        Self::color_to_rgba(self.cram[cram_index])
    }

    /// Render one plane pixel for a scroll plane.
    /// Returns `Some((rgba, priority))` if the pixel is non-transparent.
    #[must_use]
    fn render_plane_pixel(
        &self,
        nametable_base: usize,
        pixel_x: u16,
        pixel_y: u16,
        h_cells: u16,
        v_cells: u16,
    ) -> Option<([u8; 4], bool)> {
        // Which tile in the nametable
        let tile_col = (pixel_x / 8) % h_cells;
        let tile_row = (pixel_y / 8) % v_cells;
        // Nametable entry address
        let nt_offset = (tile_row * h_cells + tile_col) as usize * 2;
        let nt_addr = nametable_base + nt_offset;
        let entry = self.vram_read_word(nt_addr);

        let priority = entry & 0x8000 != 0;
        let palette = ((entry >> 13) & 0x03) as u8;
        let vflip = entry & 0x1000 != 0;
        let hflip = entry & 0x0800 != 0;
        let tile_index = entry & 0x07FF;

        let row_in_tile = (pixel_y % 8) as u8;
        let col_in_tile = (pixel_x % 8) as u8;

        let color_index = self.tile_pixel(tile_index, row_in_tile, col_in_tile, hflip, vflip);
        if color_index == 0 {
            return None; // Transparent
        }

        Some((self.resolve_color(palette, color_index), priority))
    }

    /// Renders one scanline into the framebuffer.
    ///
    /// For each of the visible pixels:
    /// 1. Fill with background color
    /// 2. Render Scroll B plane (lowest priority)
    /// 3. Render Scroll A / Window plane (window replaces Scroll A in active region)
    /// 4. Render sprites
    /// 5. Handle priority: high-priority tiles/sprites draw over low-priority
    pub fn render_scanline(&mut self, line: u16) {
        // Latch the active display width once at the start of the frame so the
        // whole frame uses one consistent stride even if reg 0x0C is written
        // mid-frame; a mode change only takes effect at the next frame boundary.
        if line == 0 {
            self.frame_width = self.screen_width();
        }
        let width = self.frame_width;
        let stride = width as usize;
        let active_height = self.active_height();
        if line as usize >= active_height || !self.display_enabled() {
            // During V-blank or if display disabled, fill with background.
            let bg = self.background_color();
            let y = line as usize;
            if y < active_height {
                for x in 0..stride {
                    let offset = (y * stride + x) * 4;
                    self.framebuffer[offset..offset + 4].copy_from_slice(&bg);
                }
            }
            return;
        }

        let y = line as usize;

        // Per-pixel compositing buffers
        // We use a simple layered approach with a priority scheme.
        // pixel_color[x] = final RGBA
        // pixel_priority[x] = priority level of the winning pixel
        //   0 = background only
        //   1 = low-priority plane/sprite pixel
        //   2 = high-priority plane/sprite pixel
        let mut pixel_color = [[0u8; 4]; 320];
        let mut pixel_priority = [0u8; 320];
        // Per-pixel operator-sprite modifier: 0 = none, 1 = shadow op, 2 = highlight op.
        // Only populated by render_sprites_on_line when `sh` is true.
        let mut sh_op = [0u8; 320];

        // ---- Steps 1-3: backdrop + Scroll B + Scroll A/Window ----
        //
        // The plane layers are composited across the beam. On the common line no
        // mid-line write was recorded, so a SINGLE full-width span reproduces the
        // original atomic render byte-for-byte (FAST PATH). When mid-line events
        // exist, the line is split into spans: render from the start-of-line
        // baseline, apply each recorded change at its dot, and render the next
        // span — so a palette / backdrop / scroll / VSRAM write part-way across
        // the line is reflected only in the pixels to its right (SPAN PATH).
        if self.line_events.is_empty() || self.line_baseline.is_none() {
            // FAST PATH — byte-identical to the pre-mid-line atomic render.
            let hscroll = self.hscroll_for_line(line);
            self.render_planes_span(line, 0, width, hscroll, &mut pixel_color, &mut pixel_priority);
        } else {
            // SPAN PATH. Save end-of-line state, rewind the tracked quantities to
            // the baseline, walk events applying each at its dot, then restore
            // the end-of-line state (so sprites / S/H / left-column below use the
            // final register/CRAM values).
            let saved_registers = self.registers;
            let saved_cram = self.cram;
            let saved_vsram = self.vsram;
            let baseline = self
                .line_baseline
                .take()
                .expect("baseline present when events recorded");
            let events = std::mem::take(&mut self.line_events);

            self.registers = baseline.registers;
            self.cram = baseline.cram;
            self.vsram = baseline.vsram;
            let mut hscroll = baseline.hscroll;
            let mut span_start: u16 = 0;
            for ev in &events {
                let dot = ev.dot.min(width);
                self.render_planes_span(
                    line,
                    span_start,
                    dot,
                    hscroll,
                    &mut pixel_color,
                    &mut pixel_priority,
                );
                match ev.change {
                    LineChange::Register { reg, value } => {
                        self.registers[reg as usize] = value;
                    }
                    LineChange::Cram { index, value } => {
                        self.cram[index as usize] = value;
                    }
                    LineChange::Vsram { index, value } => {
                        if (index as usize) < VSRAM_ENTRIES {
                            self.vsram[index as usize] = value;
                        }
                    }
                    LineChange::Hscroll { plane, value } => {
                        if plane == 0 {
                            hscroll.0 = value as i16;
                        } else {
                            hscroll.1 = value as i16;
                        }
                    }
                }
                span_start = dot;
            }
            // Final span from the last event to the right edge.
            self.render_planes_span(
                line,
                span_start,
                width,
                hscroll,
                &mut pixel_color,
                &mut pixel_priority,
            );

            // Restore the end-of-line state and hand the (now-empty of interest)
            // event buffer back so its capacity is retained for the next line.
            self.registers = saved_registers;
            self.cram = saved_cram;
            self.vsram = saved_vsram;
            self.line_events = events;
        }

        // Shadow/highlight mode gate (reg 0x0C bit 3). When disabled, rendering
        // is byte-identical to a build without S/H support (no operator
        // special-casing, plain framebuffer copy at writeback). Sprites and the
        // S/H writeback use END-OF-LINE CRAM / registers: the supported mid-line
        // split is backdrop / plane / palette across the beam for the scroll
        // layers, while sprite colors and the S/H pass sample the final CRAM
        // (a documented approximation — mid-line CRAM changes are reflected in
        // plane pixels per span but not retroactively in sprites).
        let sh = self.registers[0x0C] & 0x08 != 0;
        let bg_color = self.background_color();

        // Step 4: Sprites
        self.render_sprites_on_line(
            line,
            width,
            &mut pixel_color,
            &mut pixel_priority,
            sh,
            &mut sh_op,
        );

        // Step 5: Left column blank (register 0, bit 5)
        if self.registers[0] & 0x20 != 0 {
            for pixel in pixel_color.iter_mut().take(8) {
                *pixel = bg_color;
            }
        }

        // Write final pixel data to framebuffer
        if sh {
            // Shadow/highlight: derive per-pixel base intensity from the winning
            // pixel's priority, fold in any operator-sprite modifier, then apply.
            for (x, color) in pixel_color.iter().take(stride).enumerate() {
                // Base: high-priority winners are Normal, everything else Shadow.
                let base: u8 = if pixel_priority[x] == 2 { 1 } else { 0 };
                let intensity = match (base, sh_op[x]) {
                    (_, 0) => base,                    // no operator
                    (0, 2) => 1,                       // Shadow  + Highlight op -> Normal
                    (1, 2) => 2,                       // Normal  + Highlight op -> Highlight
                    (2, 2) => 2,                       // Highlight + Highlight op -> Highlight
                    (2, 1) => 1,                       // Highlight + Shadow op -> Normal
                    (1, 1) => 0,                       // Normal  + Shadow op -> Shadow
                    (0, 1) => 0,                       // Shadow  + Shadow op -> Shadow
                    _ => base,
                };
                let out = Self::apply_intensity(*color, intensity);
                let offset = (y * stride + x) * 4;
                self.framebuffer[offset..offset + 4].copy_from_slice(&out);
            }
        } else {
            for (x, color) in pixel_color.iter().take(stride).enumerate() {
                let offset = (y * stride + x) * 4;
                self.framebuffer[offset..offset + 4].copy_from_slice(color);
            }
        }

        self.in_hblank = false;
    }

    /// Composites the backdrop + Scroll B + Scroll A/Window planes into the
    /// pixel buffers for the half-open pixel range `[x0, x1)` of `line`.
    ///
    /// This is exactly the original render_scanline Steps 1-3, parameterised by
    /// an x span and the horizontal-scroll pair to use (passed in rather than
    /// re-derived, because HSCROLL for a span comes from the mid-line-tracked
    /// value, not a fresh VRAM read). Every other input — backdrop color, scroll
    /// sizes, nametable bases, VSRAM, CRAM, window geometry — is read from
    /// `self`, so the span reflects whatever register/CRAM/VSRAM state the caller
    /// has installed for this span. Called once at full width on the fast path
    /// (byte-identical to the atomic render) and once per span on the span path.
    #[allow(clippy::too_many_arguments)]
    fn render_planes_span(
        &self,
        line: u16,
        x0: u16,
        x1: u16,
        hscroll: (i16, i16),
        pixel_color: &mut [[u8; 4]; 320],
        pixel_priority: &mut [u8; 320],
    ) {
        let width = self.frame_width;
        let x0 = x0.min(width);
        let x1 = x1.min(width);
        if x0 >= x1 {
            return;
        }

        let (h_cells, v_cells) = self.scroll_size();
        let (hscroll_a, hscroll_b) = hscroll;
        let nt_a = self.scroll_a_nametable_addr();
        let nt_b = self.scroll_b_nametable_addr();
        let bg_color = self.background_color();

        // Step 1: Background fill
        for x in x0..x1 {
            pixel_color[x as usize] = bg_color;
        }

        // Step 2: Scroll B (lowest priority plane)
        for x in x0..x1 {
            let vscroll_b = self.vscroll_for_column(x / 8, 1);
            let plane_y = line.wrapping_add(vscroll_b) % (v_cells * 8);
            // H-scroll: the scroll value is SUBTRACTED (scrolls right = positive value
            // moves the view left). Genesis H-scroll is a positive offset = scroll left.
            let plane_x = (x as i16).wrapping_sub(hscroll_b) as u16 % (h_cells * 8);

            if let Some((color, pri)) =
                self.render_plane_pixel(nt_b, plane_x, plane_y, h_cells, v_cells)
            {
                let xi = x as usize;
                let pri_level = if pri { 2 } else { 1 };
                // Scroll B can overwrite background
                if pri_level >= pixel_priority[xi] {
                    pixel_color[xi] = color;
                    pixel_priority[xi] = pri_level;
                }
            }
        }

        // Step 3: Scroll A + Window plane
        // The window plane REPLACES Scroll A in its active region.
        // Compute window coverage for this scanline.
        let (win_left, win_right) = self.window_h_range(width);
        let (win_top, win_bottom) = self.window_v_range();
        let window_active_on_line = line >= win_top && line < win_bottom;
        let nt_win = self.window_nametable_addr();
        let win_nt_width = self.window_nametable_width();

        for x in x0..x1 {
            let xi = x as usize;
            let in_window = window_active_on_line && x >= win_left && x < win_right;

            if in_window {
                // Window plane: does NOT scroll, coordinates are screen-relative

                let tile_col = x / 8;
                let tile_row = line / 8;
                let nt_offset = (tile_row * win_nt_width + tile_col) as usize * 2;
                let nt_addr = nt_win + nt_offset;
                let entry = self.vram_read_word(nt_addr);

                let priority = entry & 0x8000 != 0;
                let palette = ((entry >> 13) & 0x03) as u8;
                let vflip = entry & 0x1000 != 0;
                let hflip = entry & 0x0800 != 0;
                let tile_index = entry & 0x07FF;

                let row_in_tile = (line % 8) as u8;
                let col_in_tile = (x % 8) as u8;

                let color_index =
                    self.tile_pixel(tile_index, row_in_tile, col_in_tile, hflip, vflip);
                if color_index != 0 {
                    // Non-transparent window pixel replaces Scroll A
                    let pri_level = if priority { 2 } else { 1 };
                    if pri_level >= pixel_priority[xi] {
                        pixel_color[xi] = self.resolve_color(palette, color_index);
                        pixel_priority[xi] = pri_level;
                    }
                }
                // Transparent window pixels let Scroll B (already composited) show through
            } else {
                // Outside window region: render Scroll A as normal
                let vscroll_a = self.vscroll_for_column(x / 8, 0);
                let plane_y = line.wrapping_add(vscroll_a) % (v_cells * 8);
                let plane_x = (x as i16).wrapping_sub(hscroll_a) as u16 % (h_cells * 8);

                if let Some((color, pri)) =
                    self.render_plane_pixel(nt_a, plane_x, plane_y, h_cells, v_cells)
                {
                    let pri_level = if pri { 2 } else { 1 };
                    // Scroll A draws over Scroll B at same or higher priority
                    if pri_level >= pixel_priority[xi] {
                        pixel_color[xi] = color;
                        pixel_priority[xi] = pri_level;
                    }
                }
            }
        }
    }

    /// Renders sprites that intersect the given scanline.
    fn render_sprites_on_line(
        &self,
        line: u16,
        width: u16,
        pixel_color: &mut [[u8; 4]; 320],
        pixel_priority: &mut [u8; 320],
        sh: bool,
        sh_op: &mut [u8; 320],
    ) {
        let sat_base = self.sprite_table_addr();
        let mut sprites_on_line: usize = 0;
        // Per-mode sprite limits: H40 evaluates 80 sprites/frame and 20/line,
        // H32 evaluates 64/frame and 16/line. (The 256-px/line horizontal extent
        // is already enforced by the `screen_x >= width` clip below; a separate
        // per-line sprite-dot budget is future work.)
        let (max_total, max_per_line) = if self.is_h40() {
            (MAX_SPRITES_TOTAL_H40, MAX_SPRITES_PER_LINE_H40)
        } else {
            (MAX_SPRITES_TOTAL_H32, MAX_SPRITES_PER_LINE_H32)
        };
        // Track which pixels already have a sprite — earlier sprites in the
        // link list have higher visual priority and should not be overwritten
        // by later sprites at the same priority level.
        let mut pixel_has_sprite = [false; 320];

        // Walk the sprite link list
        let mut sprite_index: u8 = 0;
        let mut sprites_visited: usize = 0;

        loop {
            if sprites_visited >= max_total {
                break;
            }
            sprites_visited += 1;

            let entry_addr = sat_base + usize::from(sprite_index) * 8;
            let word0 = self.vram_read_word(entry_addr);
            let word1 = self.vram_read_word(entry_addr + 2);
            let word2 = self.vram_read_word(entry_addr + 4);
            let word3 = self.vram_read_word(entry_addr + 6);

            let sprite_y = (word0 & 0x03FF).wrapping_sub(128);
            let v_size = ((word1 >> 8) & 0x03) as u8 + 1; // 1-4 tiles
            let h_size = ((word1 >> 10) & 0x03) as u8 + 1;
            let link = (word1 & 0x7F) as u8;

            let sprite_height = u16::from(v_size) * 8;

            // Check if this sprite intersects the current scanline
            if line >= sprite_y && line < sprite_y.wrapping_add(sprite_height) {
                if sprites_on_line >= max_per_line {
                    break; // Max sprites per line reached
                }

                let sprite_x = (word3 & 0x01FF).wrapping_sub(128);
                sprites_on_line += 1;

                // Decode sprite attributes
                let priority = word2 & 0x8000 != 0;
                let palette = ((word2 >> 13) & 0x03) as u8;
                let vflip = word2 & 0x1000 != 0;
                let hflip = word2 & 0x0800 != 0;
                let base_tile = word2 & 0x07FF;

                let row_in_sprite = (line.wrapping_sub(sprite_y)) as u8;
                let actual_row = if vflip {
                    v_size * 8 - 1 - row_in_sprite
                } else {
                    row_in_sprite
                };

                let tile_row = actual_row / 8;
                let pixel_row = actual_row % 8;

                for hcell in 0..h_size {
                    let actual_hcell = if hflip { h_size - 1 - hcell } else { hcell };
                    // Tile index: tiles are arranged column-major in multi-cell sprites
                    // tile = base + hcell * v_size + tile_row
                    let tile_index = base_tile
                        + u16::from(actual_hcell) * u16::from(v_size)
                        + u16::from(tile_row);

                    for pixel_col in 0u8..8 {
                        let screen_x =
                            sprite_x.wrapping_add(u16::from(hcell) * 8 + u16::from(pixel_col));
                        if screen_x >= width {
                            continue;
                        }

                        let actual_pcol = if hflip { 7 - pixel_col } else { pixel_col };
                        let color_index =
                            self.tile_pixel(tile_index, pixel_row, actual_pcol, false, false);
                        if color_index == 0 {
                            continue; // Transparent
                        }

                        let xi = screen_x as usize;

                        // Shadow/highlight operator sprites: palette 3, color
                        // index 14 (highlight) or 15 (shadow). They do NOT draw
                        // color or set priority; they record a modifier for the
                        // pixel behind them, but only if no color sprite has
                        // already drawn in front (front-to-back order) and no
                        // nearer operator was already recorded.
                        if sh && palette == 3 && (color_index == 14 || color_index == 15) {
                            if !pixel_has_sprite[xi] && sh_op[xi] == 0 {
                                sh_op[xi] = if color_index == 15 { 1 } else { 2 };
                            }
                            continue;
                        }

                        let pri_level = if priority { 2 } else { 1 };

                        // Sprite compositing rules:
                        // - Sprites overwrite plane pixels at same or higher priority (>=)
                        // - Earlier sprites in link list win over later sprites at
                        //   same priority (only strictly higher priority can overwrite)
                        let can_draw = if pixel_has_sprite[xi] {
                            pri_level > pixel_priority[xi]
                        } else {
                            pri_level >= pixel_priority[xi]
                        };
                        if can_draw {
                            pixel_color[xi] = self.resolve_color(palette, color_index);
                            pixel_priority[xi] = pri_level;
                            pixel_has_sprite[xi] = true;
                        }
                    }
                }
            }

            // Follow link list
            if link == 0 {
                break;
            }
            sprite_index = link;
        }
    }

    /// Snapshot for save states.
    #[must_use]
    pub fn snapshot(&self) -> VdpSnapshot {
        VdpSnapshot {
            vram: self.vram.to_vec(),
            cram: self.cram.to_vec(),
            vsram: self.vsram.to_vec(),
            registers: self.registers.to_vec(),
            control_state: self.control_state,
            address: self.address,
            scanline: self.scanline,
            dot: self.dot,
            auto_increment: self.auto_increment,
            access_type: self.access_type,
            in_vblank: self.in_vblank,
            in_hblank: self.in_hblank,
            dma_pending: self.dma_pending,
            dma_fill_pending: self.dma_fill_pending,
            h_interrupt_counter: self.h_interrupt_counter,
            h_interrupt_pending: self.h_interrupt_pending,
            control_code: self.control_code,
            odd_frame: self.odd_frame,
            vint_pending: self.vint_pending,
            dma_busy_cpu_cycles: self.dma_busy_cpu_cycles,
            region: self.region,
        }
    }

    /// Restores VDP state from a snapshot. The framebuffer is left untouched
    /// (it is re-derived when subsequent scanlines are rendered).
    pub fn restore(&mut self, snap: &VdpSnapshot) {
        self.vram.copy_from_slice(&snap.vram);
        self.cram.copy_from_slice(&snap.cram);
        self.vsram.copy_from_slice(&snap.vsram);
        self.registers.copy_from_slice(&snap.registers);
        self.control_state = snap.control_state;
        self.address = snap.address;
        self.scanline = snap.scanline;
        self.dot = snap.dot;
        self.auto_increment = snap.auto_increment;
        self.access_type = snap.access_type;
        self.in_vblank = snap.in_vblank;
        self.in_hblank = snap.in_hblank;
        self.dma_pending = snap.dma_pending;
        self.dma_fill_pending = snap.dma_fill_pending;
        self.h_interrupt_counter = snap.h_interrupt_counter;
        self.h_interrupt_pending = snap.h_interrupt_pending;
        self.control_code = snap.control_code;
        self.odd_frame = snap.odd_frame;
        self.vint_pending = snap.vint_pending;
        self.dma_busy_cpu_cycles = snap.dma_busy_cpu_cycles;
        self.region = snap.region;
        // frame_width is a derived per-frame cache (not serialized). Recompute
        // it from the restored registers so the returned framebuffer length is
        // coherent even before the next frame renders.
        self.frame_width = self.screen_width();
    }
}

impl Default for Vdp {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // NTSC nominal active-scanline count, used by the H-interrupt VBlank-reload
    // test. Non-test code uses the region-aware `active_height()` instead.
    use crate::timing::ACTIVE_SCANLINES;

    #[test]
    fn register_write() {
        let mut vdp = Vdp::new();
        // Write 0x42 to register 5: command = 0x8542
        vdp.write_control(0x8542, None);
        assert_eq!(vdp.registers[5], 0x42);
    }

    #[test]
    fn auto_increment_register() {
        let mut vdp = Vdp::new();
        // Set auto-increment (reg 0x0F) to 2
        vdp.write_control(0x8F02, None);
        assert_eq!(vdp.auto_increment, 2);
    }

    #[test]
    fn vram_write_and_read() {
        let mut vdp = Vdp::new();
        // Set auto-increment to 2
        vdp.write_control(0x8F02, None);
        // Set up VRAM write to address 0x0000
        // First word:  CD1-0=01 (VRAM write), A13-0=0x0000 -> 0x4000
        // Second word: CD5-2=0000, A15-14=00 -> 0x0000
        vdp.write_control(0x4000, None);
        vdp.write_control(0x0000, None);
        // Write data
        vdp.write_data(0xABCD, None);

        // Set up VRAM read from address 0x0000
        vdp.write_control(0x0000, None);
        vdp.write_control(0x0000, None);
        let val = vdp.read_data();
        assert_eq!(val, 0xABCD);
    }

    #[test]
    fn cram_write_masks_to_9bit() {
        let mut vdp = Vdp::new();
        vdp.write_control(0x8F02, None);
        // CRAM write: CD=0011 -> first word has CD1-0=11, addr=0
        vdp.write_control(0xC000, None);
        vdp.write_control(0x0000, None);
        vdp.write_data(0xFFFF, None);
        // Should be masked to 0x0EEE
        vdp.write_control(0x0000, None); // CRAM read setup
        vdp.write_control(0x0020, None); // CD5-2=1000
        // Direct check via snapshot
        let snap = vdp.snapshot();
        assert_eq!(snap.cram[0], 0x0EEE);
    }

    #[test]
    fn color_conversion() {
        // Full white: 0x0EEE -> (7,7,7) -> (255,255,255) approximately
        let rgba = Vdp::color_to_rgba(0x0EEE);
        assert_eq!(rgba[0], 255); // R
        assert_eq!(rgba[1], 255); // G
        assert_eq!(rgba[2], 255); // B

        // Pure red: 0x000E
        let rgba = Vdp::color_to_rgba(0x000E);
        assert_eq!(rgba[0], 255);
        assert_eq!(rgba[1], 0);
        assert_eq!(rgba[2], 0);

        // Black: 0x0000
        let rgba = Vdp::color_to_rgba(0x0000);
        assert_eq!(rgba[0], 0);
        assert_eq!(rgba[1], 0);
        assert_eq!(rgba[2], 0);
    }

    #[test]
    fn status_register_vblank_flag() {
        let mut vdp = Vdp::new();
        assert_eq!(vdp.read_status() & 0x0008, 0);
        vdp.in_vblank = true;
        assert_ne!(vdp.read_status() & 0x0008, 0);
    }

    // ---- Rendering tests ----

    /// Helper: set up VDP for basic rendering (display enabled, H40 mode,
    /// 32x32 scroll size, auto-increment 2).
    fn setup_vdp_for_rendering() -> Vdp {
        let mut vdp = Vdp::new();
        // Register 1: display enable (bit 6) + DMA enable (bit 4)
        vdp.registers[1] = 0x74; // 0b0111_0100
        // Register 0x0C: H40 mode (bit 7)
        vdp.registers[0x0C] = 0x81;
        // Register 0x10: scroll size 32x32 (0x00)
        vdp.registers[0x10] = 0x00;
        // Register 0x0F: auto-increment = 2
        vdp.registers[0x0F] = 0x02;
        vdp.auto_increment = 2;
        // Scroll A nametable at 0xC000 (reg 2 = 0x30 -> bits 5-3 = 0b110 -> 6 * 0x2000 = 0xC000)
        vdp.registers[0x02] = 0x30;
        // Scroll B nametable at 0xE000 (reg 4 = 0x07 -> 7 * 0x2000 = 0xE000)
        vdp.registers[0x04] = 0x07;
        // Sprite table at 0xD800 (reg 5 = 0x6C -> 0x6C * 0x200 = 0xD800)
        vdp.registers[0x05] = 0x6C;
        // H-scroll table at 0xDC00 (reg 0x0D = 0x37 -> 0x37 * 0x400 = 0xDC00)
        vdp.registers[0x0D] = 0x37;
        // Background color: palette 0, color 0 (black by default)
        vdp.registers[0x07] = 0x00;
        // H-scroll mode: full screen
        vdp.registers[0x0B] = 0x00;
        vdp
    }

    /// Helper: write a 16-bit word to VRAM at a given address (big-endian).
    fn vram_write_word(vdp: &mut Vdp, addr: usize, value: u16) {
        let a = addr & (VRAM_SIZE - 1);
        let a1 = (a + 1) & (VRAM_SIZE - 1);
        vdp.vram[a] = (value >> 8) as u8;
        vdp.vram[a1] = value as u8;
    }

    /// Helper: write a tile pattern (8x8, 4bpp = 32 bytes) to VRAM.
    /// `rows` contains 8 rows of 8 pixel indices (4-bit each).
    fn write_tile_pattern(vdp: &mut Vdp, tile_index: u16, rows: &[[u8; 8]; 8]) {
        let base = usize::from(tile_index) * 32;
        for (row_idx, row) in rows.iter().enumerate() {
            let row_addr = base + row_idx * 4;
            for col_pair in 0..4 {
                let left = row[col_pair * 2] & 0x0F;
                let right = row[col_pair * 2 + 1] & 0x0F;
                vdp.vram[(row_addr + col_pair) & (VRAM_SIZE - 1)] = (left << 4) | right;
            }
        }
    }

    #[test]
    fn background_color_fills_framebuffer() {
        let mut vdp = setup_vdp_for_rendering();
        // Set background to palette 1, color 2 -> CRAM index 18
        vdp.registers[0x07] = 0x12; // palette 1, color 2
        // Set CRAM[18] to bright green: 0x00E0
        vdp.cram[18] = 0x00E0;

        // All nametable tiles are 0 (tile 0, palette 0, no priority) and
        // tile 0's pattern is all zeros (transparent), so background shows through.
        vdp.render_scanline(0);

        // Check pixel (0, 0) is the background color (green)
        let expected = Vdp::color_to_rgba(0x00E0);
        assert_eq!(vdp.framebuffer[0], expected[0]); // R
        assert_eq!(vdp.framebuffer[1], expected[1]); // G
        assert_eq!(vdp.framebuffer[2], expected[2]); // B
        assert_eq!(vdp.framebuffer[3], 0xFF); // A

        // Check another pixel mid-screen
        let offset = 160 * 4;
        assert_eq!(vdp.framebuffer[offset], expected[0]);
        assert_eq!(vdp.framebuffer[offset + 1], expected[1]);
    }

    #[test]
    fn simple_tile_renders_correctly() {
        let mut vdp = setup_vdp_for_rendering();

        // Put a solid color in palette 0, color 1
        // Red: 0x000E
        vdp.cram[1] = 0x000E;

        // Write a tile pattern at tile index 1: all pixels = color 1
        let solid_tile = [[1u8; 8]; 8];
        write_tile_pattern(&mut vdp, 1, &solid_tile);

        // Write nametable entry at position (0,0) in Scroll A to point to tile 1
        // No priority, palette 0, no flip, tile index 1
        let nt_a_base = vdp.scroll_a_nametable_addr();
        vram_write_word(&mut vdp, nt_a_base, 0x0001); // tile index 1

        // Render scanline 0
        vdp.render_scanline(0);

        // Pixels 0-7 on line 0 should be red (tile 1, palette 0, color 1)
        let expected = Vdp::color_to_rgba(0x000E);
        for x in 0..8usize {
            let offset = x * 4;
            assert_eq!(
                &vdp.framebuffer[offset..offset + 4],
                &expected,
                "pixel at x={x} should be red"
            );
        }

        // Pixel 8 should be background (tile 0 is transparent -> BG)
        let bg = vdp.background_color();
        let offset = 8 * 4;
        assert_eq!(
            &vdp.framebuffer[offset..offset + 4],
            &bg,
            "pixel at x=8 should be background"
        );
    }

    #[test]
    fn tile_horizontal_flip() {
        let mut vdp = setup_vdp_for_rendering();

        // Palette 0, color 1 = red, color 2 = blue
        vdp.cram[1] = 0x000E; // red
        vdp.cram[2] = 0x0E00; // blue

        // Tile 1: left half red, right half blue
        let mut pattern = [[0u8; 8]; 8];
        for row in &mut pattern {
            row[..4].fill(1); // red
            row[4..8].fill(2); // blue
        }
        write_tile_pattern(&mut vdp, 1, &pattern);

        // Nametable entry: tile 1, hflip set (bit 11)
        let nt_a_base = vdp.scroll_a_nametable_addr();
        vram_write_word(&mut vdp, nt_a_base, 0x0801); // hflip + tile 1

        vdp.render_scanline(0);

        // With hflip, left half should be blue, right half should be red
        let blue = Vdp::color_to_rgba(0x0E00);
        let red = Vdp::color_to_rgba(0x000E);
        for x in 0..4usize {
            let offset = x * 4;
            assert_eq!(
                &vdp.framebuffer[offset..offset + 4],
                &blue,
                "pixel at x={x} should be blue (flipped)"
            );
        }
        for x in 4..8usize {
            let offset = x * 4;
            assert_eq!(
                &vdp.framebuffer[offset..offset + 4],
                &red,
                "pixel at x={x} should be red (flipped)"
            );
        }
    }

    #[test]
    fn priority_high_overwrites_low() {
        let mut vdp = setup_vdp_for_rendering();

        vdp.cram[1] = 0x000E; // red (palette 0, color 1)
        vdp.cram[2] = 0x0E00; // blue (palette 0, color 2)

        // Tile 1: solid color 1 (red) - for Scroll B
        let solid_red = [[1u8; 8]; 8];
        write_tile_pattern(&mut vdp, 1, &solid_red);

        // Tile 2: solid color 2 (blue) - for Scroll A
        let solid_blue = [[2u8; 8]; 8];
        write_tile_pattern(&mut vdp, 2, &solid_blue);

        // Scroll B nametable: tile 1, HIGH priority (bit 15)
        let nt_b_base = vdp.scroll_b_nametable_addr();
        vram_write_word(&mut vdp, nt_b_base, 0x8001); // priority + tile 1

        // Scroll A nametable: tile 2, LOW priority
        let nt_a_base = vdp.scroll_a_nametable_addr();
        vram_write_word(&mut vdp, nt_a_base, 0x0002); // no priority + tile 2

        vdp.render_scanline(0);

        // Scroll B has high priority, Scroll A has low priority.
        // Scroll A at low priority (level 1) should overwrite Scroll B at
        // high priority (level 2)? No: Scroll B is processed first, sets
        // priority level 2. Scroll A at level 1 cannot overwrite level 2.
        // So the pixel should be RED (Scroll B wins with high priority).
        let expected = Vdp::color_to_rgba(0x000E);
        assert_eq!(&vdp.framebuffer[0..4], &expected);
    }

    #[test]
    fn vblank_flag_set_at_correct_scanline() {
        let mut vdp = Vdp::new();

        // Before any scanlines, not in vblank
        assert!(!vdp.in_vblank());

        // Simulate scanlines 0..223 (active)
        for line in 0..224 {
            vdp.begin_scanline(line);
        }
        // Still not in vblank (set externally by core)
        assert!(!vdp.in_vblank());

        // Core sets vblank at scanline 224
        vdp.set_vblank(true);
        assert!(vdp.in_vblank());
        assert_ne!(vdp.read_status() & 0x0008, 0);

        // End frame clears it (actually core does via set_vblank(false))
        vdp.set_vblank(false);
        assert!(!vdp.in_vblank());
    }

    #[test]
    fn begin_scanline_sets_hblank_and_scanline() {
        let mut vdp = Vdp::new();
        vdp.registers[0x0A] = 0x0A; // H-interrupt counter reload value

        vdp.begin_scanline(0);
        assert_eq!(vdp.scanline(), 0);
        assert!(vdp.in_hblank);
        assert_eq!(vdp.h_interrupt_counter, 10); // Reloaded at line 0

        vdp.begin_scanline(42);
        assert_eq!(vdp.scanline(), 42);
        assert!(vdp.in_hblank);
    }

    #[test]
    fn end_frame_resets_scanline() {
        let mut vdp = Vdp::new();
        vdp.scanline = 200;
        vdp.in_hblank = true;
        vdp.end_frame();
        assert_eq!(vdp.scanline(), 0);
        assert!(!vdp.in_hblank);
    }

    #[test]
    fn display_disabled_renders_background() {
        let mut vdp = Vdp::new();
        // Display disabled (reg 1 bit 6 clear)
        vdp.registers[1] = 0x04;
        // Set background color
        vdp.registers[0x07] = 0x00;
        vdp.cram[0] = 0x0EEE; // white

        vdp.render_scanline(0);

        let expected = Vdp::color_to_rgba(0x0EEE);
        assert_eq!(&vdp.framebuffer[0..4], &expected);
    }

    // ---- DMA tests ----

    #[test]
    fn dma_68k_to_vram() {
        let mut vdp = setup_vdp_for_rendering();

        // Set up DMA source registers: address 0x000100 (word address = 0x80)
        // Source byte address = word_addr << 1 = 0x000100
        vdp.registers[0x15] = 0x80; // low byte of word address
        vdp.registers[0x16] = 0x00; // mid byte
        vdp.registers[0x17] = 0x00; // high byte (bits 6-7 = 00 = 68K to VRAM)

        // DMA length: 4 words
        vdp.registers[0x13] = 0x04;
        vdp.registers[0x14] = 0x00;

        // Set destination to VRAM address 0x0000
        vdp.address = 0x0000;
        vdp.access_type = Some(AccessType::VramWrite);
        vdp.dma_pending = true;

        // Provide a source data callback (simulated ROM)
        let mut source_data: Vec<u8> = vec![0; 0x200];
        source_data[0x100] = 0xAB;
        source_data[0x101] = 0xCD;
        source_data[0x102] = 0x12;
        source_data[0x103] = 0x34;
        source_data[0x104] = 0x56;
        source_data[0x105] = 0x78;
        source_data[0x106] = 0x9A;
        source_data[0x107] = 0xBC;

        let source = source_data.clone();
        let mut read_word = |addr: u32| -> u16 {
            let offset = addr as usize;
            if offset + 1 < source.len() {
                u16::from(source[offset]) << 8 | u16::from(source[offset + 1])
            } else {
                0
            }
        };

        vdp.run_dma(&mut read_word);

        // Verify VRAM contents
        assert!(!vdp.dma_pending());
        assert_eq!(vdp.vram[0], 0xAB);
        assert_eq!(vdp.vram[1], 0xCD);
        assert_eq!(vdp.vram[2], 0x12);
        assert_eq!(vdp.vram[3], 0x34);
        assert_eq!(vdp.vram[4], 0x56);
        assert_eq!(vdp.vram[5], 0x78);
        assert_eq!(vdp.vram[6], 0x9A);
        assert_eq!(vdp.vram[7], 0xBC);

        // Length registers should be zeroed
        assert_eq!(vdp.registers[0x13], 0);
        assert_eq!(vdp.registers[0x14], 0);
    }

    #[test]
    fn dma_vram_fill() {
        let mut vdp = setup_vdp_for_rendering();

        // Set DMA length to 8 words
        vdp.registers[0x13] = 0x08;
        vdp.registers[0x14] = 0x00;
        // DMA mode: VRAM fill (reg 0x17 bits 7-6 = 10)
        vdp.registers[0x17] = 0x80;

        // Set destination address and access type
        vdp.address = 0x0000;
        vdp.access_type = Some(AccessType::VramWrite);
        vdp.dma_fill_pending = true;

        // The fill value comes from data port write
        // High byte (0xFF) gets written to each address
        vdp.write_data(0xFF00, None);

        // First word should be the full write value
        assert_eq!(vdp.vram[0], 0xFF);
        assert_eq!(vdp.vram[1], 0x00);
        // Remaining 7 addresses should have the high byte (0xFF)
        for i in 1..8 {
            let addr = i * 2; // auto-increment is 2
            assert_eq!(vdp.vram[addr], 0xFF, "VRAM fill at addr {addr}");
        }

        // DMA fill pending should be cleared
        assert!(!vdp.dma_fill_pending);
        // Length registers should be zeroed
        assert_eq!(vdp.registers[0x13], 0);
        assert_eq!(vdp.registers[0x14], 0);
    }

    #[test]
    fn dma_trigger_via_control_port() {
        let mut vdp = setup_vdp_for_rendering();

        // Set up DMA registers for 68K-to-VRAM
        vdp.registers[0x13] = 0x10; // length = 16 words
        vdp.registers[0x14] = 0x00;
        vdp.registers[0x15] = 0x00; // source word address 0
        vdp.registers[0x16] = 0x00;
        vdp.registers[0x17] = 0x00; // mode = 00 (68K to VRAM)

        // DMA requires reg 1 bit 4 set (already done in setup_vdp_for_rendering)
        assert!(vdp.registers[1] & 0x10 != 0);

        // Two-word control port command with CD5 set:
        // First word: CD1-0=01 (VRAM write), address low bits
        // Second word: CD5=1 (DMA), remaining address/CD bits
        // CD = 0b10_0001 = 0x21 (VRAM write + DMA)
        // First word: bits 15-14 = CD1-0 = 01, bits 13-0 = addr low = 0
        // -> first = 0x4000
        // Second word: bits 7-2 = CD5-2 = 1000_00, bits 1-0 = addr high = 00
        // -> second = 0x0080
        vdp.write_control(0x4000, None);
        vdp.write_control(0x0080, None);

        // DMA should now be pending
        assert!(vdp.dma_pending());
    }

    #[test]
    fn scroll_b_renders_behind_scroll_a() {
        let mut vdp = setup_vdp_for_rendering();

        vdp.cram[1] = 0x000E; // red
        vdp.cram[2] = 0x00E0; // green

        // Tile 1: solid red
        write_tile_pattern(&mut vdp, 1, &[[1u8; 8]; 8]);
        // Tile 2: solid green
        write_tile_pattern(&mut vdp, 2, &[[2u8; 8]; 8]);

        // Scroll B: tile 1 (red), no priority
        let nt_b = vdp.scroll_b_nametable_addr();
        vram_write_word(&mut vdp, nt_b, 0x0001);

        // Scroll A: tile 2 (green), no priority
        let nt_a = vdp.scroll_a_nametable_addr();
        vram_write_word(&mut vdp, nt_a, 0x0002);

        vdp.render_scanline(0);

        // Scroll A should overwrite Scroll B (both low priority, A drawn after B)
        let expected = Vdp::color_to_rgba(0x00E0); // green
        assert_eq!(&vdp.framebuffer[0..4], &expected);
    }

    #[test]
    fn transparent_tile_shows_layer_below() {
        let mut vdp = setup_vdp_for_rendering();

        vdp.cram[1] = 0x000E; // red

        // Tile 1: solid red
        write_tile_pattern(&mut vdp, 1, &[[1u8; 8]; 8]);

        // Scroll B: tile 1 (red)
        let nt_b = vdp.scroll_b_nametable_addr();
        vram_write_word(&mut vdp, nt_b, 0x0001);

        // Scroll A: tile 0 (all transparent — default)
        // Already 0 by default

        vdp.render_scanline(0);

        // Scroll A is transparent, so Scroll B (red) shows through
        let expected = Vdp::color_to_rgba(0x000E);
        assert_eq!(&vdp.framebuffer[0..4], &expected);
    }

    #[test]
    fn tile_pixel_extracts_nibbles_correctly() {
        let mut vdp = Vdp::new();

        // Write tile 0 with a specific pattern
        // Row 0: pixels [0xA, 0xB, 0xC, 0xD, 0x1, 0x2, 0x3, 0x4]
        let row0 = [0xA, 0xB, 0xC, 0xD, 0x1, 0x2, 0x3, 0x4];
        let mut pattern = [[0u8; 8]; 8];
        pattern[0] = row0;
        write_tile_pattern(&mut vdp, 0, &pattern);

        for (col, &expected) in row0.iter().enumerate() {
            let got = vdp.tile_pixel(0, 0, col as u8, false, false);
            assert_eq!(got, expected, "tile_pixel at col {col}");
        }
    }

    #[test]
    fn scroll_size_decoding() {
        let mut vdp = Vdp::new();

        vdp.registers[0x10] = 0x00; // 32x32
        assert_eq!(vdp.scroll_size(), (32, 32));

        vdp.registers[0x10] = 0x01; // 64x32
        assert_eq!(vdp.scroll_size(), (64, 32));

        vdp.registers[0x10] = 0x11; // 64x64
        assert_eq!(vdp.scroll_size(), (64, 64));

        vdp.registers[0x10] = 0x33; // 128x128
        assert_eq!(vdp.scroll_size(), (128, 128));

        // Invalid (0b10) treated as 32
        vdp.registers[0x10] = 0x22;
        assert_eq!(vdp.scroll_size(), (32, 32));
    }

    // ---- Window plane tests ----

    #[test]
    fn window_plane_renders_over_scroll_a() {
        let mut vdp = setup_vdp_for_rendering();

        // Set up colors: palette 0 color 1 = red, palette 0 color 2 = green
        vdp.cram[1] = 0x000E; // red
        vdp.cram[2] = 0x00E0; // green

        // Tile 1: solid red (for Scroll A)
        write_tile_pattern(&mut vdp, 1, &[[1u8; 8]; 8]);
        // Tile 2: solid green (for window)
        write_tile_pattern(&mut vdp, 2, &[[2u8; 8]; 8]);

        // Fill Scroll A nametable with tile 1 (red) across the full row
        let nt_a = vdp.scroll_a_nametable_addr();
        let (h_cells, _) = vdp.scroll_size();
        for col in 0..h_cells {
            vram_write_word(&mut vdp, nt_a + col as usize * 2, 0x0001);
        }

        // Window nametable at 0xA000 (reg 0x03 = 0x28 -> 0x28 & 0x3E = 0x28, 0x28 << 10 = 0xA000)
        vdp.registers[0x03] = 0x28;
        let nt_win = vdp.window_nametable_addr();
        assert_eq!(nt_win, 0xA000);

        // Fill window nametable with tile 2 (green) across the full row
        // Window nametable width in H40 mode = 64 cells
        let win_nt_width: u16 = 64;
        for col in 0..win_nt_width {
            vram_write_word(&mut vdp, nt_win + col as usize * 2, 0x0002);
        }

        // Configure window: left boundary at 160 px (10 cells * 16 px/unit = 160 px),
        // full vertical.
        // Register 0x11: left side (bit 7 = 0), count = 10 (0x0A)
        vdp.registers[0x11] = 0x0A;
        // Register 0x12: full vertical (bit 7 = 0), cell count = 31 (0x1F) -> 248 lines, covers 224
        vdp.registers[0x12] = 0x1F;

        vdp.render_scanline(0);

        // Pixel 0 should be green (window plane)
        let green = Vdp::color_to_rgba(0x00E0);
        assert_eq!(
            &vdp.framebuffer[0..4],
            &green,
            "pixel 0 should be green (window)"
        );

        // Pixel 160 should be red (Scroll A, outside window)
        let red = Vdp::color_to_rgba(0x000E);
        let offset = 160 * 4;
        assert_eq!(
            &vdp.framebuffer[offset..offset + 4],
            &red,
            "pixel 160 should be red (scroll A outside window)"
        );
    }

    #[test]
    fn window_plane_disabled_shows_scroll_a() {
        let mut vdp = setup_vdp_for_rendering();

        // Set up color: palette 0 color 1 = red
        vdp.cram[1] = 0x000E; // red

        // Tile 1: solid red
        write_tile_pattern(&mut vdp, 1, &[[1u8; 8]; 8]);

        // Scroll A nametable: tile 1 at position (0,0)
        let nt_a = vdp.scroll_a_nametable_addr();
        vram_write_word(&mut vdp, nt_a, 0x0001);

        // Window registers at 0 — no coverage
        // Register 0x11 = 0x00: left side, 0 cells -> (0, 0) range = empty
        vdp.registers[0x11] = 0x00;
        // Register 0x12 = 0x00: top side, 0 cells -> (0, 0) range = empty
        vdp.registers[0x12] = 0x00;

        vdp.render_scanline(0);

        // Pixel 0 should be red (Scroll A, no window active)
        let red = Vdp::color_to_rgba(0x000E);
        assert_eq!(
            &vdp.framebuffer[0..4],
            &red,
            "pixel 0 should be red (scroll A, no window)"
        );
    }

    #[test]
    fn window_plane_right_side() {
        let mut vdp = setup_vdp_for_rendering();

        // Set up colors: palette 0 color 1 = red, palette 0 color 2 = green
        vdp.cram[1] = 0x000E; // red
        vdp.cram[2] = 0x00E0; // green

        // Tile 1: solid red (for Scroll A)
        write_tile_pattern(&mut vdp, 1, &[[1u8; 8]; 8]);
        // Tile 2: solid green (for window)
        write_tile_pattern(&mut vdp, 2, &[[2u8; 8]; 8]);

        // Fill Scroll A nametable with tile 1 (red) across full row
        let nt_a = vdp.scroll_a_nametable_addr();
        let (h_cells, _) = vdp.scroll_size();
        for col in 0..h_cells {
            vram_write_word(&mut vdp, nt_a + col as usize * 2, 0x0001);
        }

        // Window nametable at 0xA000
        vdp.registers[0x03] = 0x28;
        let nt_win = vdp.window_nametable_addr();

        // Fill window nametable with tile 2 (green) across full row
        let win_nt_width: u16 = 64;
        for col in 0..win_nt_width {
            vram_write_word(&mut vdp, nt_win + col as usize * 2, 0x0002);
        }

        // Configure window on right side from x=160 rightward, full vertical
        // (10 cells * 16 px/unit = 160 px boundary).
        // Register 0x11: right side (bit 7 = 1), count = 10 -> 0x80 | 0x0A = 0x8A
        vdp.registers[0x11] = 0x8A;
        // Register 0x12: full vertical coverage
        vdp.registers[0x12] = 0x1F;

        vdp.render_scanline(0);

        // Pixel 0 should be red (Scroll A, left of window)
        let red = Vdp::color_to_rgba(0x000E);
        assert_eq!(
            &vdp.framebuffer[0..4],
            &red,
            "pixel 0 should be red (scroll A, left of window)"
        );

        // Pixel 160 should be green (window, right side)
        let green = Vdp::color_to_rgba(0x00E0);
        let offset = 160 * 4;
        assert_eq!(
            &vdp.framebuffer[offset..offset + 4],
            &green,
            "pixel 160 should be green (window, right side)"
        );
    }

    #[test]
    fn h_interrupt_counter_fires_at_zero() {
        let mut vdp = Vdp::new();
        vdp.registers[0x0A] = 0x03; // fire every 4 scanlines
        vdp.registers[0] = 0x10; // enable H-interrupt

        vdp.begin_scanline(0); // counter loaded with 3
        assert!(!vdp.h_interrupt_pending());

        vdp.begin_scanline(1); // counter = 2
        assert!(!vdp.h_interrupt_pending());

        vdp.begin_scanline(2); // counter = 1
        assert!(!vdp.h_interrupt_pending());

        vdp.begin_scanline(3); // counter = 0
        assert!(!vdp.h_interrupt_pending());

        vdp.begin_scanline(4); // counter was 0, now -1 < 0, fire!
        assert!(vdp.h_interrupt_pending());
        vdp.clear_h_interrupt();

        vdp.begin_scanline(5); // counter reloaded to 3, decremented to 2
        assert!(!vdp.h_interrupt_pending());
    }

    #[test]
    fn h_interrupt_disabled_does_not_fire() {
        let mut vdp = Vdp::new();
        vdp.registers[0x0A] = 0x00; // fire every scanline
        vdp.registers[0] = 0x00; // H-interrupt DISABLED

        vdp.begin_scanline(0);
        vdp.begin_scanline(1); // would fire if enabled
        assert!(!vdp.h_interrupt_pending());
    }

    // ---- HV counter tests ----

    #[test]
    fn hv_counter_reflects_scanline() {
        let mut vdp = Vdp::new();
        vdp.begin_scanline(42);
        let hv = vdp.read_hv_counter(0);
        assert_eq!((hv >> 8) as u8, 42, "V counter should be scanline number");
    }

    #[test]
    fn hv_counter_v_wrap_ntsc() {
        let mut vdp = Vdp::new();
        // Scanline 0xEB (235) should wrap: 235 - 6 = 229 = 0xE5
        vdp.scanline = 0xEB;
        let hv = vdp.read_hv_counter(0);
        assert_eq!((hv >> 8) as u8, 0xE5);
    }

    #[test]
    fn hv_counter_h_increases_across_line_h40() {
        let mut vdp = Vdp::new();
        // H40 mode (reg 0x0C bit 0/7 set) — 320px.
        vdp.registers[0x0C] = 0x81;
        vdp.begin_scanline(10);

        let line = MASTER_TICKS_PER_LINE_H40;
        let h_start = (vdp.read_hv_counter(0) & 0xFF) as u8;
        let h_quarter = (vdp.read_hv_counter(line / 4) & 0xFF) as u8;
        let h_mid = (vdp.read_hv_counter(line / 2) & 0xFF) as u8;
        let h_end = (vdp.read_hv_counter(line - 1) & 0xFF) as u8;

        // Start of line is the low end of the counter.
        assert_eq!(h_start, 0x00, "H counter starts at 0x00");
        // H rises monotonically through the visible portion.
        assert!(
            h_start < h_quarter && h_quarter < h_mid,
            "H counter should increase across the line: {h_start:#x} < {h_quarter:#x} < {h_mid:#x}"
        );
        // By end of line the beam is past the mid-line jump, in the 0xE4..=0xFF
        // range (H40), and strictly higher than the pre-jump visible maximum.
        assert!(
            h_end >= 0xE4,
            "H counter at end of H40 line should be in the post-jump 0xE4..=0xFF range, got {h_end:#x}"
        );
        // The documented visible maximum for H40 is 0xB6; the post-jump value
        // exceeds it, and the just-before-jump region should reach up near 0xB6.
        assert!(h_end > 0xB6, "post-jump H exceeds visible max 0xB6");
    }

    #[test]
    fn hv_counter_h_jump_h32() {
        let mut vdp = Vdp::new();
        // H32 mode (reg 0x0C bits clear) — 256px.
        vdp.registers[0x0C] = 0x00;
        vdp.begin_scanline(10);
        let h_end = (vdp.read_hv_counter(MASTER_TICKS_PER_LINE_H40 - 1) & 0xFF) as u8;
        // H32 post-jump range is 0xE9..=0xFF.
        assert!(
            h_end >= 0xE9,
            "H counter at end of H32 line should be in 0xE9..=0xFF, got {h_end:#x}"
        );
    }

    #[test]
    fn h_interrupt_counter_reloads_during_vblank() {
        let mut vdp = Vdp::new();
        vdp.registers[0x0A] = 0x05; // HINT period
        vdp.registers[0] = 0x10; // enable H-interrupt

        // Run through several active lines so the counter is mid-count (not at
        // its reload value).
        vdp.begin_scanline(0); // reload -> 5
        vdp.begin_scanline(1); // -> 4
        vdp.begin_scanline(2); // -> 3
        assert_eq!(
            vdp.h_interrupt_counter, 3,
            "counter should be decremented mid-frame"
        );

        // Enter VBlank (line >= ACTIVE_SCANLINES): the counter must reload from
        // reg 0x0A even though we never hit line 0.
        vdp.begin_scanline(ACTIVE_SCANLINES);
        assert_eq!(
            vdp.h_interrupt_counter,
            i16::from(vdp.registers[0x0A]),
            "H-interrupt counter should reload from reg 0x0A during VBlank"
        );

        // A change to reg 0x0A during VBlank takes effect on the next VBlank line.
        vdp.registers[0x0A] = 0x07;
        vdp.begin_scanline(ACTIVE_SCANLINES + 10);
        assert_eq!(
            vdp.h_interrupt_counter, 0x07,
            "reload during VBlank tracks the current reg 0x0A value"
        );
    }

    // ---- Status register completeness tests ----

    #[test]
    fn status_register_fifo_empty_set() {
        let vdp = Vdp::new();
        assert_ne!(
            vdp.read_status() & 0x0200,
            0,
            "FIFO empty bit should be set"
        );
    }

    #[test]
    fn status_register_odd_frame_toggles() {
        let mut vdp = Vdp::new();
        let status1 = vdp.read_status();
        vdp.end_frame();
        let status2 = vdp.read_status();
        assert_ne!(
            status1 & 0x0010,
            status2 & 0x0010,
            "odd frame bit should toggle"
        );
    }

    // ---- Shadow / highlight tests ----

    /// A distinctive mid-gray (all three 3-bit components = 4).
    /// Normal = [146,146,146,255], Shadow = [73,73,73,255], Highlight = [201,201,201,255].
    const SH_GRAY: u16 = 0x0888;

    /// Enable shadow/highlight mode (reg 0x0C bit 3) while keeping H40.
    fn enable_shadow_highlight(vdp: &mut Vdp) {
        vdp.registers[0x0C] |= 0x08;
    }

    /// Write a 1x1 sprite as the sole entry of the sprite attribute table,
    /// positioned at screen (0,0). The sprite tile is filled with `color_index`.
    fn write_single_sprite(vdp: &mut Vdp, tile: u16, palette: u8, priority: bool, color_index: u8) {
        write_tile_pattern(vdp, tile, &[[color_index; 8]; 8]);
        let sat = vdp.sprite_table_addr();
        // word0: Y raw 128 -> screen Y 0
        vram_write_word(vdp, sat, 0x0080);
        // word1: v_size=1, h_size=1, link=0 (end of list)
        vram_write_word(vdp, sat + 2, 0x0000);
        // word2: priority | palette | tile
        let pri_bit = if priority { 0x8000 } else { 0x0000 };
        let pal_bits = (u16::from(palette) & 0x03) << 13;
        vram_write_word(vdp, sat + 4, pri_bit | pal_bits | (tile & 0x07FF));
        // word3: X raw 128 -> screen X 0
        vram_write_word(vdp, sat + 6, 0x0080);
    }

    #[test]
    fn shadow_highlight_disabled_is_unchanged() {
        let mut vdp = setup_vdp_for_rendering();
        // reg 0x0C bit 3 stays 0 (S/H disabled).
        vdp.cram[1] = SH_GRAY;
        write_tile_pattern(&mut vdp, 1, &[[1u8; 8]; 8]);
        // Scroll A: low-priority tile 1.
        let nt_a = vdp.scroll_a_nametable_addr();
        vram_write_word(&mut vdp, nt_a, 0x0001);

        vdp.render_scanline(0);

        let normal = Vdp::color_to_rgba(SH_GRAY);
        assert_eq!(
            &vdp.framebuffer[0..4],
            &normal,
            "S/H disabled: low-priority plane must render at full normal color"
        );
    }

    #[test]
    fn shadow_highlight_low_priority_plane_is_shadowed() {
        let mut vdp = setup_vdp_for_rendering();
        enable_shadow_highlight(&mut vdp);
        vdp.cram[1] = SH_GRAY;
        write_tile_pattern(&mut vdp, 1, &[[1u8; 8]; 8]);
        let nt_a = vdp.scroll_a_nametable_addr();
        vram_write_word(&mut vdp, nt_a, 0x0001); // low priority

        vdp.render_scanline(0);

        let normal = Vdp::color_to_rgba(SH_GRAY);
        let shadow = Vdp::apply_intensity(normal, 0);
        assert_eq!(shadow[0], normal[0] >> 1, "shadow halves the component");
        assert_eq!(
            &vdp.framebuffer[0..4],
            &shadow,
            "S/H on: low-priority plane pixel must be shadowed (halved)"
        );
    }

    #[test]
    fn shadow_highlight_high_priority_plane_is_normal() {
        let mut vdp = setup_vdp_for_rendering();
        enable_shadow_highlight(&mut vdp);
        vdp.cram[1] = SH_GRAY;
        write_tile_pattern(&mut vdp, 1, &[[1u8; 8]; 8]);
        let nt_a = vdp.scroll_a_nametable_addr();
        vram_write_word(&mut vdp, nt_a, 0x8001); // HIGH priority

        vdp.render_scanline(0);

        let normal = Vdp::color_to_rgba(SH_GRAY);
        assert_eq!(
            &vdp.framebuffer[0..4],
            &normal,
            "S/H on: high-priority plane pixel stays at full normal color"
        );
    }

    #[test]
    fn shadow_highlight_backdrop_is_shadowed() {
        let mut vdp = setup_vdp_for_rendering();
        enable_shadow_highlight(&mut vdp);
        // Backdrop = palette 0 color 0; give it a visible value.
        vdp.cram[0] = SH_GRAY;
        vdp.registers[0x07] = 0x00; // background = CRAM index 0

        // No tiles anywhere -> whole scanline is backdrop (priority 0).
        vdp.render_scanline(0);

        let normal = Vdp::color_to_rgba(SH_GRAY);
        let shadow = Vdp::apply_intensity(normal, 0);
        assert_eq!(
            &vdp.framebuffer[0..4],
            &shadow,
            "S/H on: backdrop (priority 0) must be shadowed"
        );
    }

    #[test]
    fn shadow_operator_sprite_darkens() {
        let mut vdp = setup_vdp_for_rendering();
        enable_shadow_highlight(&mut vdp);
        // High-priority (Normal) plane pixel behind the operator.
        vdp.cram[1] = SH_GRAY;
        write_tile_pattern(&mut vdp, 1, &[[1u8; 8]; 8]);
        let nt_a = vdp.scroll_a_nametable_addr();
        vram_write_word(&mut vdp, nt_a, 0x8001); // high priority -> Normal base

        // Sentinel: if the operator wrongly drew its own color it would show red.
        vdp.cram[63] = 0x000E; // palette 3 color 15 -> red
        // Operator sprite: palette 3, color index 15 (shadow operator).
        write_single_sprite(&mut vdp, 3, 3, false, 15);

        vdp.render_scanline(0);

        let normal = Vdp::color_to_rgba(SH_GRAY);
        let shadow = Vdp::apply_intensity(normal, 0);
        assert_eq!(
            &vdp.framebuffer[0..4],
            &shadow,
            "shadow operator over a Normal pixel yields Shadow"
        );
        // Prove the operator did NOT draw its own (red) color.
        let sentinel = Vdp::color_to_rgba(0x000E);
        assert_ne!(
            &vdp.framebuffer[0..4],
            &sentinel,
            "operator sprite must not draw its own color"
        );
    }

    #[test]
    fn highlight_operator_sprite_brightens() {
        let normal = Vdp::color_to_rgba(SH_GRAY);

        // Case (a): highlight operator over a SHADOWED (low-priority) pixel -> Normal.
        {
            let mut vdp = setup_vdp_for_rendering();
            enable_shadow_highlight(&mut vdp);
            vdp.cram[1] = SH_GRAY;
            write_tile_pattern(&mut vdp, 1, &[[1u8; 8]; 8]);
            let nt_a = vdp.scroll_a_nametable_addr();
            vram_write_word(&mut vdp, nt_a, 0x0001); // low priority -> Shadow base
            // Operator sprite: palette 3, color index 14 (highlight operator).
            write_single_sprite(&mut vdp, 3, 3, false, 14);

            vdp.render_scanline(0);
            assert_eq!(
                &vdp.framebuffer[0..4],
                &normal,
                "highlight operator over a Shadow pixel yields Normal"
            );
        }

        // Case (b): highlight operator over a NORMAL (high-priority) pixel -> Highlight.
        {
            let mut vdp = setup_vdp_for_rendering();
            enable_shadow_highlight(&mut vdp);
            vdp.cram[1] = SH_GRAY;
            write_tile_pattern(&mut vdp, 1, &[[1u8; 8]; 8]);
            let nt_a = vdp.scroll_a_nametable_addr();
            vram_write_word(&mut vdp, nt_a, 0x8001); // high priority -> Normal base
            write_single_sprite(&mut vdp, 3, 3, false, 14);

            vdp.render_scanline(0);
            let highlight = Vdp::apply_intensity(normal, 2);
            assert_eq!(
                &vdp.framebuffer[0..4],
                &highlight,
                "highlight operator over a Normal pixel yields Highlight"
            );
            assert!(
                vdp.framebuffer[0] > normal[0],
                "highlight must increase brightness"
            );
        }
    }

    #[test]
    fn operator_sprite_draws_as_color_when_sh_disabled() {
        let mut vdp = setup_vdp_for_rendering();
        // S/H DISABLED (bit 3 left at 0).
        // palette 3 color 15 = a distinct color.
        vdp.cram[63] = 0x000E; // red
        write_single_sprite(&mut vdp, 3, 3, false, 15);

        vdp.render_scanline(0);

        let red = Vdp::color_to_rgba(0x000E);
        assert_eq!(
            &vdp.framebuffer[0..4],
            &red,
            "with S/H disabled, a palette-3 index-15 sprite draws its actual color"
        );
    }

    #[test]
    fn normal_low_priority_sprite_over_low_bg_is_shadowed() {
        let mut vdp = setup_vdp_for_rendering();
        enable_shadow_highlight(&mut vdp);
        // Low-priority plane behind (red), distinct from sprite color.
        vdp.cram[1] = 0x000E; // red
        write_tile_pattern(&mut vdp, 1, &[[1u8; 8]; 8]);
        let nt_a = vdp.scroll_a_nametable_addr();
        vram_write_word(&mut vdp, nt_a, 0x0001); // low priority

        // Normal (non-operator) low-priority sprite, palette 0 color 2 = gray.
        vdp.cram[2] = SH_GRAY;
        write_single_sprite(&mut vdp, 3, 0, false, 2);

        vdp.render_scanline(0);

        let normal = Vdp::color_to_rgba(SH_GRAY);
        let shadow = Vdp::apply_intensity(normal, 0);
        assert_eq!(
            &vdp.framebuffer[0..4],
            &shadow,
            "low-priority sprite over low-priority bg: winning pixel is shadowed"
        );
    }

    #[test]
    fn high_priority_sprite_is_normal() {
        let mut vdp = setup_vdp_for_rendering();
        enable_shadow_highlight(&mut vdp);
        vdp.cram[2] = SH_GRAY;
        // High-priority, non-operator sprite (palette 0 color 2).
        write_single_sprite(&mut vdp, 3, 0, true, 2);

        vdp.render_scanline(0);

        let normal = Vdp::color_to_rgba(SH_GRAY);
        assert_eq!(
            &vdp.framebuffer[0..4],
            &normal,
            "high-priority sprite renders at full normal color"
        );
    }

    // ---- DMA-busy / VInt-latch / status bits (regression tests for the fix) ----

    /// The DMA-busy status bit (bit 1) is not set at reset and does not appear
    /// in `read_status` until a transfer charges its cycle countdown.
    #[test]
    fn dma_busy_defaults_clear() {
        let vdp = Vdp::new();
        assert_eq!(vdp.dma_busy_cpu_cycles(), 0);
        assert!(!vdp.dma_busy());
        assert_eq!(vdp.read_status() & 0x0002, 0);
    }

    /// `dma_cost_cycles` charges more cycles in H40 than in H32, and more
    /// during active display than during blanking (rates from the Sega
    /// Software Manual DMA timing table).
    #[test]
    fn dma_cost_reflects_mode_and_blanking() {
        let mut vdp = Vdp::new();
        // H40 mode, display enabled → active-display rates.
        vdp.registers[0x0C] = 0x81;
        vdp.registers[1] = 0x40;
        vdp.in_vblank = false;
        let h40_active = vdp.dma_cost_cycles(1000, false);
        // H40, blanking (display off).
        vdp.registers[1] = 0x00;
        let h40_blank = vdp.dma_cost_cycles(1000, false);
        // H32 blanking runs slower per line than H40 blanking.
        vdp.registers[0x0C] = 0x00;
        let h32_blank = vdp.dma_cost_cycles(1000, false);

        assert!(
            h40_active > h40_blank,
            "active-display DMA stalls the CPU for more cycles than blanking (h40_active={h40_active}, h40_blank={h40_blank})"
        );
        assert!(
            h32_blank > h40_blank,
            "H32 blanking DMA is slower per line than H40 blanking (h32_blank={h32_blank}, h40_blank={h40_blank})"
        );

        // Zero-length transfers cost nothing.
        assert_eq!(vdp.dma_cost_cycles(0, false), 0);
        // Copy DMA runs at half the rate → costs more than a normal move of
        // the same length.
        let move_cost = vdp.dma_cost_cycles(1000, false);
        let copy_cost = vdp.dma_cost_cycles(1000, true);
        assert!(copy_cost >= move_cost);
    }

    /// A pending 68K→VRAM DMA charges DMA-busy for the transfer's cycle cost;
    /// the status register reflects it; `advance_dma_busy` drains the countdown
    /// and clears the bit.
    #[test]
    fn dma_busy_asserted_by_run_dma_and_drains_to_zero() {
        let mut vdp = setup_vdp_for_rendering();
        // 8-word transfer from address 0 into VRAM at 0xE000.
        vdp.registers[0x13] = 0x08;
        vdp.registers[0x14] = 0x00;
        vdp.registers[0x15] = 0x00;
        vdp.registers[0x16] = 0x00;
        vdp.registers[0x17] = 0x00;
        vdp.address = 0xE000;
        vdp.access_type = Some(AccessType::VramWrite);
        vdp.dma_pending = true;

        vdp.run_dma(&mut |_addr| 0xBEEF);

        let cost = vdp.dma_busy_cpu_cycles();
        assert!(cost > 0, "run_dma must charge DMA-busy cycles");
        assert!(vdp.dma_busy());
        assert_ne!(
            vdp.read_status() & 0x0002,
            0,
            "status register bit 1 (DMA busy) is set while the transfer holds the bus"
        );

        // Drain part-way: still busy.
        vdp.advance_dma_busy(cost / 2);
        assert!(vdp.dma_busy());

        // Drain the rest: DMA-busy clears.
        vdp.advance_dma_busy(cost);
        assert!(!vdp.dma_busy());
        assert_eq!(vdp.read_status() & 0x0002, 0);
    }

    /// A VRAM fill also charges DMA-busy and stalls the bus.
    #[test]
    fn dma_fill_asserts_dma_busy() {
        let mut vdp = setup_vdp_for_rendering();
        vdp.registers[0x13] = 0x40;
        vdp.registers[0x14] = 0x00;
        vdp.registers[0x17] = 0x80; // fill
        vdp.address = 0x0000;
        vdp.access_type = Some(AccessType::VramWrite);

        vdp.execute_dma_fill(0xABCD);

        assert!(vdp.dma_busy_cpu_cycles() > 0);
        assert_ne!(vdp.read_status() & 0x0002, 0);
    }

    /// The V-interrupt-pending (VIP) latch: setting it exposes status bit 7;
    /// clearing it hides it again.
    #[test]
    fn vint_pending_latch_shows_in_status_bit_7() {
        let mut vdp = Vdp::new();
        assert!(!vdp.vint_pending());
        assert_eq!(vdp.read_status() & 0x0080, 0);

        vdp.set_vint_pending();
        assert!(vdp.vint_pending());
        assert_ne!(
            vdp.read_status() & 0x0080,
            0,
            "status bit 7 (V-interrupt pending) tracks the latched flag"
        );

        vdp.clear_vint_pending();
        assert!(!vdp.vint_pending());
        assert_eq!(vdp.read_status() & 0x0080, 0);
    }

    /// Snapshot round-trip preserves the new fields — mirrors the coverage the
    /// rewind determinism test in `genesoxide-test-harness` relies on.
    #[test]
    fn snapshot_roundtrip_covers_vint_and_dma_busy() {
        let mut vdp = Vdp::new();
        vdp.set_vint_pending();
        vdp.dma_busy_cpu_cycles = 1234;

        let snap = vdp.snapshot();
        assert!(snap.vint_pending);
        assert_eq!(snap.dma_busy_cpu_cycles, 1234);

        let mut restored = Vdp::new();
        restored.restore(&snap);
        assert!(restored.vint_pending());
        assert_eq!(restored.dma_busy_cpu_cycles(), 1234);
    }
}

//! VGM (Video Game Music) parser, builder, and renderer.
//!
//! VGM files contain exact chip register writes with sample-accurate timing
//! (at 44100 Hz). This module supports:
//!
//! - **Parsing** `.vgm` and `.vgz` (gzipped) files
//! - **Building** VGM data programmatically for targeted unit tests
//! - **Rendering** VGM data through the genesoxide YM2612 to produce audio samples
//!
//! # VGM Format Reference
//!
//! Commands used for YM2612:
//! - `0x52 aa dd` — Write register `aa` with value `dd` to port 0 (channels 1-3)
//! - `0x53 aa dd` — Write register `aa` with value `dd` to port 1 (channels 4-6)
//! - `0x50 dd`    — SN76489 (PSG) write
//! - `0x61 nn nn` — Wait `n` samples (16-bit LE)
//! - `0x62`       — Wait 735 samples (1/60 sec NTSC)
//! - `0x63`       — Wait 882 samples (1/50 sec PAL)
//! - `0x66`       — End of sound data
//! - `0x70-0x7F`  — Wait 1-16 samples
//! - `0x80-0x8F`  — YM2612 DAC write from data bank + wait 0-15 samples

use genesoxide_core::api::{AudioFilterSpec, AudioOutputConfig, TimedPsgWrite, TimedYm2612Write};
use genesoxide_core::psg::Psg;
use genesoxide_core::scheduler::MASTER_CLOCK_NTSC;
use genesoxide_core::ym2612::Ym2612;
use std::io::Read;
use std::path::Path;

const MAX_POST_DELAY_SAMPLES: usize = 4;

// ── VGM Header ──────────────────────────────────────────────────────────

/// Parsed VGM file header.
#[derive(Debug, Clone)]
pub struct VgmHeader {
    /// Total number of samples (at 44100 Hz) in the file.
    pub total_samples: u32,
    /// Loop offset (0 = no loop).
    pub loop_offset: u32,
    /// Loop sample count.
    pub loop_samples: u32,
    /// YM2612 clock frequency (0 = not used). NTSC Genesis = 7_670_453.
    pub ym2612_clock: u32,
    /// SN76489 clock frequency (0 = not used). NTSC Genesis = 3_579_545.
    pub sn76489_clock: u32,
    /// Offset to VGM data (absolute byte position in the file).
    pub data_offset: usize,
    /// VGM version (e.g., 0x150 = version 1.50).
    pub version: u32,
}

// ── VGM Commands ────────────────────────────────────────────────────────

/// A single VGM command, parsed from the data stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VgmCommand {
    /// Write to YM2612 port 0: register `reg`, value `val`.
    Ym2612Port0 { reg: u8, val: u8 },
    /// Write to YM2612 port 1: register `reg`, value `val`.
    Ym2612Port1 { reg: u8, val: u8 },
    /// Write to SN76489 (PSG).
    Psg { val: u8 },
    /// Wait `samples` samples (at 44100 Hz).
    Wait { samples: u16 },
    /// YM2612 DAC write from data bank + wait. `offset` is the byte position
    /// in the data bank; `wait` is additional samples to wait (0-15).
    DacWrite { offset: u32, wait: u8 },
    /// End of sound data.
    End,
    /// Unknown/unsupported command (skip).
    Unknown { cmd: u8 },
}

// ── VGM Parser ──────────────────────────────────────────────────────────

/// Parsed VGM file: header + command list.
#[derive(Debug, Clone)]
pub struct Vgm {
    pub header: VgmHeader,
    pub commands: Vec<VgmCommand>,
    /// YM2612 DAC data bank (from 0x67 data blocks with type 0x00).
    pub dac_data: Vec<u8>,
}

impl Vgm {
    /// Parse a VGM file from raw bytes. Handles both `.vgm` and `.vgz` (gzip).
    pub fn parse(data: &[u8]) -> Result<Self, String> {
        // Check for gzip magic bytes
        let data = if data.len() >= 2 && data[0] == 0x1F && data[1] == 0x8B {
            let mut decoder = flate2::read::GzDecoder::new(data);
            let mut decompressed = Vec::new();
            decoder
                .read_to_end(&mut decompressed)
                .map_err(|e| format!("gzip decompression failed: {e}"))?;
            decompressed
        } else {
            data.to_vec()
        };

        if data.len() < 0x40 {
            return Err("VGM file too small for header".into());
        }

        // Check magic: "Vgm "
        if &data[0..4] != b"Vgm " {
            return Err(format!("Invalid VGM magic: {:?}", &data[0..4]));
        }

        let version = read_u32_le(&data, 0x08);
        let sn76489_clock = read_u32_le(&data, 0x0C);
        let total_samples = read_u32_le(&data, 0x18);
        let loop_offset_raw = read_u32_le(&data, 0x1C);
        let loop_samples = read_u32_le(&data, 0x20);
        let ym2612_clock = read_u32_le(&data, 0x2C);

        let loop_offset = if loop_offset_raw != 0 {
            loop_offset_raw + 0x1C
        } else {
            0
        };

        // VGM data offset: for version >= 1.50, stored at 0x34 (relative to 0x34).
        // For older versions, data starts at 0x40.
        let data_offset = if version >= 0x150 {
            let rel = read_u32_le(&data, 0x34);
            if rel == 0 {
                0x40
            } else {
                (0x34 + rel) as usize
            }
        } else {
            0x40
        };

        let header = VgmHeader {
            total_samples,
            loop_offset,
            loop_samples,
            ym2612_clock,
            sn76489_clock,
            data_offset,
            version,
        };

        // Parse commands and extract DAC data bank
        let mut dac_data = Vec::new();
        let commands = parse_commands(&data, data_offset, &mut dac_data);

        Ok(Vgm {
            header,
            commands,
            dac_data,
        })
    }

    /// Load and parse a VGM/VGZ file from disk.
    pub fn load(path: &Path) -> Result<Self, String> {
        let data =
            std::fs::read(path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
        Self::parse(&data)
    }

    /// Total duration in seconds.
    pub fn duration_secs(&self) -> f64 {
        self.header.total_samples as f64 / 44100.0
    }
}

/// Convert Genesis master-clock ticks to 44.1 kHz output samples.
#[must_use]
pub fn samples_from_master_ticks(master_ticks: u64) -> u32 {
    ((u128::from(master_ticks) * 44_100u128 + u128::from(MASTER_CLOCK_NTSC / 2))
        / u128::from(MASTER_CLOCK_NTSC)) as u32
}

fn master_ticks_from_output_samples(samples: u32) -> u64 {
    ((u128::from(samples) * u128::from(MASTER_CLOCK_NTSC) + 22_050u128) / 44_100u128) as u64
}

/// Converts timed live YM2612 and PSG writes into a replayable VGM stream.
pub fn vgm_from_timed_sound_writes(
    ym_writes: &[TimedYm2612Write],
    psg_writes: &[TimedPsgWrite],
    start_tick: u64,
    end_tick: u64,
) -> Vgm {
    let mut commands = Vec::with_capacity((ym_writes.len() + psg_writes.len()) * 2 + 2);
    let mut emitted_samples = 0u32;
    let mut ym_idx = 0usize;
    let mut psg_idx = 0usize;

    while ym_idx < ym_writes.len() || psg_idx < psg_writes.len() {
        let next_is_ym = match (ym_writes.get(ym_idx), psg_writes.get(psg_idx)) {
            (Some(ym), Some(psg)) => ym.master_tick <= psg.master_tick,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };

        let (event_tick, command) = if next_is_ym {
            let write = ym_writes[ym_idx];
            ym_idx += 1;
            let command = match write.port {
                0 => VgmCommand::Ym2612Port0 {
                    reg: write.addr,
                    val: write.value,
                },
                _ => VgmCommand::Ym2612Port1 {
                    reg: write.addr,
                    val: write.value,
                },
            };
            (write.master_tick, command)
        } else {
            let write = psg_writes[psg_idx];
            psg_idx += 1;
            (write.master_tick, VgmCommand::Psg { val: write.value })
        };

        let rel_tick = event_tick.saturating_sub(start_tick);
        let target_samples = samples_from_master_ticks(rel_tick);
        let wait = target_samples.saturating_sub(emitted_samples);
        if wait > 0 {
            let mut remaining = wait;
            while remaining > 0 {
                let chunk = remaining.min(u16::MAX as u32) as u16;
                commands.push(VgmCommand::Wait { samples: chunk });
                remaining -= u32::from(chunk);
            }
            emitted_samples = target_samples;
        }

        commands.push(command);
    }

    let total_samples = samples_from_master_ticks(end_tick.saturating_sub(start_tick));
    let tail_wait = total_samples.saturating_sub(emitted_samples);
    if tail_wait > 0 {
        let mut remaining = tail_wait;
        while remaining > 0 {
            let chunk = remaining.min(u16::MAX as u32) as u16;
            commands.push(VgmCommand::Wait { samples: chunk });
            remaining -= u32::from(chunk);
        }
    }
    commands.push(VgmCommand::End);

    Vgm {
        header: VgmHeader {
            total_samples,
            loop_offset: 0,
            loop_samples: 0,
            ym2612_clock: 7_670_453,
            sn76489_clock: 3_579_545,
            data_offset: 0,
            version: 0x150,
        },
        commands,
        dac_data: Vec::new(),
    }
}

/// Converts timed live YM2612 writes into a replayable VGM stream.
pub fn vgm_from_timed_ym2612_writes(
    writes: &[TimedYm2612Write],
    start_tick: u64,
    end_tick: u64,
) -> Vgm {
    vgm_from_timed_sound_writes(writes, &[], start_tick, end_tick)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ym2612TrackedEventKind {
    KeyOn,
    KeyOff,
    ToneChange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ym2612TrackedEvent {
    pub sample: u32,
    pub channel: u8,
    pub kind: Ym2612TrackedEventKind,
    pub freq_hz: Option<f64>,
}

fn push_ym2612_tracked_event(events: &mut Vec<Ym2612TrackedEvent>, event: Ym2612TrackedEvent) {
    if event.kind == Ym2612TrackedEventKind::ToneChange
        && events.last().is_some_and(|last| {
            last.sample == event.sample
                && last.channel == event.channel
                && last.kind == Ym2612TrackedEventKind::ToneChange
        })
    {
        *events
            .last_mut()
            .expect("tone-change check already ensured last") = event;
        return;
    }

    events.push(event);
}

fn soundlog_document_from_vgm(vgm: &Vgm) -> Result<soundlog::VgmDocument, String> {
    use soundlog::VgmBuilder as SoundlogVgmBuilder;
    use soundlog::chip;
    use soundlog::vgm::command::{Instance, WaitSamples};

    let mut builder = SoundlogVgmBuilder::new();
    if vgm.header.ym2612_clock > 0 {
        builder.register_chip(
            chip::Chip::Ym2612,
            Instance::Primary,
            vgm.header.ym2612_clock,
        );
    }
    if vgm.header.sn76489_clock > 0 {
        builder.register_chip(
            chip::Chip::Sn76489,
            Instance::Primary,
            vgm.header.sn76489_clock,
        );
    }

    for command in &vgm.commands {
        match *command {
            VgmCommand::Ym2612Port0 { reg, val } => {
                builder.add_chip_write(
                    Instance::Primary,
                    chip::Ym2612Spec {
                        port: 0,
                        register: reg,
                        value: val,
                    },
                );
            }
            VgmCommand::Ym2612Port1 { reg, val } => {
                builder.add_chip_write(
                    Instance::Primary,
                    chip::Ym2612Spec {
                        port: 1,
                        register: reg,
                        value: val,
                    },
                );
            }
            VgmCommand::Psg { val } => {
                builder.add_chip_write(Instance::Primary, chip::PsgSpec { value: val });
            }
            VgmCommand::Wait { samples } => {
                builder.add_vgm_command(WaitSamples(samples));
            }
            VgmCommand::End => {}
            VgmCommand::DacWrite { .. } | VgmCommand::Unknown { .. } => {
                return Err(format!(
                    "soundlog document conversion does not support command {:?}",
                    command
                ));
            }
        }
    }

    Ok(builder.finalize())
}

/// Extract structured YM2612 musical events from timed live writes using the
/// `soundlog` chip-state tracker.
pub fn extract_ym2612_state_events_from_timed_writes(
    writes: &[TimedYm2612Write],
    start_tick: u64,
    end_tick: u64,
) -> Result<Vec<Ym2612TrackedEvent>, String> {
    use soundlog::VgmCallbackStream;
    use soundlog::chip::event::StateEvent;
    use soundlog::chip::state::Ym2612State;
    use soundlog::chip::{self};
    use soundlog::vgm::command::Instance;

    let vgm = vgm_from_timed_ym2612_writes(writes, start_tick, end_tick);
    let document = soundlog_document_from_vgm(&vgm)?;
    let mut extracted_events = Vec::new();

    {
        let mut callback_stream = VgmCallbackStream::from_document(document);
        callback_stream
            .track_state::<Ym2612State>(Instance::Primary, vgm.header.ym2612_clock as f32);
        callback_stream.on_write(|_inst, _spec: chip::Ym2612Spec, sample, event_opt| {
            if let Some(events) = event_opt {
                for event in events {
                    match event {
                        StateEvent::KeyOn { channel, tone } => {
                            push_ym2612_tracked_event(
                                &mut extracted_events,
                                Ym2612TrackedEvent {
                                    sample: sample as u32,
                                    channel,
                                    kind: Ym2612TrackedEventKind::KeyOn,
                                    freq_hz: tone.freq_hz.map(f64::from),
                                },
                            );
                        }
                        StateEvent::KeyOff { channel } => {
                            push_ym2612_tracked_event(
                                &mut extracted_events,
                                Ym2612TrackedEvent {
                                    sample: sample as u32,
                                    channel,
                                    kind: Ym2612TrackedEventKind::KeyOff,
                                    freq_hz: None,
                                },
                            );
                        }
                        StateEvent::ToneChange { channel, tone } => {
                            push_ym2612_tracked_event(
                                &mut extracted_events,
                                Ym2612TrackedEvent {
                                    sample: sample as u32,
                                    channel,
                                    kind: Ym2612TrackedEventKind::ToneChange,
                                    freq_hz: tone.freq_hz.map(f64::from),
                                },
                            );
                        }
                    }
                }
            }
        });

        for result in callback_stream {
            if let Err(error) = result {
                return Err(format!("soundlog callback stream failed: {error:?}"));
            }
        }
    }

    Ok(extracted_events)
}

/// Parse the VGM command stream starting at `offset`.
/// Populates `dac_data` with type 0x00 data blocks (YM2612 PCM).
fn parse_commands(data: &[u8], mut offset: usize, dac_data: &mut Vec<u8>) -> Vec<VgmCommand> {
    let mut commands = Vec::new();
    let mut dac_stream_offset: u32 = 0;

    while offset < data.len() {
        let cmd = data[offset];
        offset += 1;

        match cmd {
            // YM2612 port 0 write
            0x52 => {
                if offset + 1 < data.len() {
                    commands.push(VgmCommand::Ym2612Port0 {
                        reg: data[offset],
                        val: data[offset + 1],
                    });
                    offset += 2;
                }
            }
            // YM2612 port 1 write
            0x53 => {
                if offset + 1 < data.len() {
                    commands.push(VgmCommand::Ym2612Port1 {
                        reg: data[offset],
                        val: data[offset + 1],
                    });
                    offset += 2;
                }
            }
            // SN76489 write
            0x50 => {
                if offset < data.len() {
                    commands.push(VgmCommand::Psg { val: data[offset] });
                    offset += 1;
                }
            }
            // Wait n samples
            0x61 => {
                if offset + 1 < data.len() {
                    let samples = u16::from_le_bytes([data[offset], data[offset + 1]]);
                    commands.push(VgmCommand::Wait { samples });
                    offset += 2;
                }
            }
            // Wait 735 samples (NTSC frame)
            0x62 => {
                commands.push(VgmCommand::Wait { samples: 735 });
            }
            // Wait 882 samples (PAL frame)
            0x63 => {
                commands.push(VgmCommand::Wait { samples: 882 });
            }
            // End of sound data
            0x66 => {
                commands.push(VgmCommand::End);
                break;
            }
            // Short wait (1-16 samples)
            0x70..=0x7F => {
                let n = (cmd & 0x0F) + 1;
                commands.push(VgmCommand::Wait { samples: n as u16 });
            }
            // YM2612 DAC write from data bank + wait 0-15 samples.
            // Reads one byte from the data bank at `dac_stream_offset` and
            // writes it to register 0x2A, then waits (cmd & 0x0F) samples.
            0x80..=0x8F => {
                let wait = cmd & 0x0F;
                commands.push(VgmCommand::DacWrite {
                    offset: dac_stream_offset,
                    wait,
                });
                dac_stream_offset += 1;
            }
            // Data block: 0x67 0x66 tt ss ss ss ss [data]
            0x67 => {
                if offset + 6 <= data.len() {
                    offset += 1; // skip 0x66
                    let block_type = data[offset];
                    offset += 1;
                    let size = read_u32_le(data, offset) as usize;
                    offset += 4;
                    // Type 0x00 = YM2612 PCM data (DAC stream bank)
                    if block_type == 0x00 && offset + size <= data.len() {
                        dac_data.extend_from_slice(&data[offset..offset + size]);
                    }
                    offset += size;
                }
            }
            // Seek in data bank (set stream offset)
            0xE0 => {
                if offset + 4 <= data.len() {
                    dac_stream_offset = read_u32_le(data, offset);
                    offset += 4;
                }
            }
            // Unknown — try to skip based on known command sizes
            _ => {
                commands.push(VgmCommand::Unknown { cmd });
                // Most 2-byte commands: 0x30-0x4F
                if (0x30..=0x4F).contains(&cmd) {
                    offset += 1;
                }
                // Most 3-byte commands: 0x51, 0x54-0x5F, 0xA0-0xBF
                else if cmd == 0x51
                    || (0x54..=0x5F).contains(&cmd)
                    || (0xA0..=0xBF).contains(&cmd)
                {
                    offset += 2;
                }
                // 4-byte commands: 0xC0-0xDF
                else if (0xC0..=0xDF).contains(&cmd) {
                    offset += 3;
                }
                // 5-byte commands: 0xE1-0xFF
                else if (0xE1..=0xFF).contains(&cmd) {
                    offset += 4;
                }
            }
        }
    }

    commands
}

fn read_u32_le(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

// ── VGM Builder ─────────────────────────────────────────────────────────

/// Programmatic VGM builder for creating test sequences.
///
/// # Example
///
/// ```ignore
/// let vgm = VgmBuilder::new()
///     .ym_write(0, 0xB0, 0x00)  // Algorithm 0, feedback 0
///     .ym_write(0, 0x40, 0x00)  // TL=0 for op1
///     .ym_write(0, 0x28, 0xF0)  // Key-on all ops, ch0
///     .wait(44100)              // 1 second
///     .ym_write(0, 0x28, 0x00)  // Key-off
///     .wait(4410)               // 0.1s release
///     .build();
/// ```
pub struct VgmBuilder {
    commands: Vec<VgmCommand>,
}

impl VgmBuilder {
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    /// Write a YM2612 register. `port` is 0 (channels 1-3) or 1 (channels 4-6).
    pub fn ym_write(mut self, port: u8, reg: u8, val: u8) -> Self {
        self.commands.push(match port {
            0 => VgmCommand::Ym2612Port0 { reg, val },
            _ => VgmCommand::Ym2612Port1 { reg, val },
        });
        self
    }

    /// Write to the PSG.
    pub fn psg_write(mut self, val: u8) -> Self {
        self.commands.push(VgmCommand::Psg { val });
        self
    }

    /// Wait for `samples` samples at 44100 Hz.
    pub fn wait(mut self, samples: u32) -> Self {
        let mut remaining = samples;
        while remaining > 0 {
            let n = remaining.min(u16::MAX as u32) as u16;
            self.commands.push(VgmCommand::Wait { samples: n });
            remaining -= n as u32;
        }
        self
    }

    /// Wait for `ms` milliseconds.
    pub fn wait_ms(self, ms: u32) -> Self {
        self.wait((44100 * ms) / 1000)
    }

    /// Convenience: set up a single operator on channel 0 to produce a tone.
    ///
    /// Sets algorithm 7 (all carriers), configures operator 1 (slot 0) with
    /// the given TL and AR, and keys on. Other operators are silenced (TL=127).
    pub fn single_op_tone(self, fnum: u16, block: u8, tl: u8, ar: u8) -> Self {
        let fnum_hi = ((block & 7) << 3) | ((fnum >> 8) & 0x07) as u8;
        let fnum_lo = (fnum & 0xFF) as u8;

        self
            // Algorithm 7 (all ops are carriers), feedback 0
            .ym_write(0, 0xB0, 0x07)
            // Panning: both L+R
            .ym_write(0, 0xB4, 0xC0)
            // Silence all ops first (TL=127)
            .ym_write(0, 0x40, 127) // op1
            .ym_write(0, 0x44, 127) // op3
            .ym_write(0, 0x48, 127) // op2
            .ym_write(0, 0x4C, 127) // op4
            // Configure op1: MUL=1, DT=0
            .ym_write(0, 0x30, 0x01)
            // TL
            .ym_write(0, 0x40, tl)
            // AR (and KS=0)
            .ym_write(0, 0x50, ar & 0x1F)
            // DR=0
            .ym_write(0, 0x60, 0x00)
            // SR=0
            .ym_write(0, 0x70, 0x00)
            // SL=0, RR=15
            .ym_write(0, 0x80, 0x0F)
            // Set frequency
            .ym_write(0, 0xA4, fnum_hi)
            .ym_write(0, 0xA0, fnum_lo)
            // Key-on op1 only (bit 4)
            .ym_write(0, 0x28, 0x10)
    }

    /// Convenience: set up two-operator FM on channel 0 (algorithm 4).
    ///
    /// Algorithm 4: (op1→op2) + (op3→op4). We use the op1→op2 pair and
    /// silence op3+op4. Op1 is the modulator, op2 is the carrier.
    ///
    /// Register slot mapping: slot 0=op1(+0x00), slot 2=op2(+0x08),
    /// slot 1=op3(+0x04), slot 3=op4(+0x0C).
    pub fn two_op_fm(
        self,
        fnum: u16,
        block: u8,
        mod_tl: u8,
        car_tl: u8,
        mod_mul: u8,
        car_mul: u8,
        feedback: u8,
    ) -> Self {
        let fnum_hi = ((block & 7) << 3) | ((fnum >> 8) & 0x07) as u8;
        let fnum_lo = (fnum & 0xFF) as u8;

        self
            // Algorithm 4 ((op1→op2)(C) + (op3→op4)(C)), feedback
            .ym_write(0, 0xB0, (feedback << 3) | 0x04)
            .ym_write(0, 0xB4, 0xC0)
            // Silence all ops
            .ym_write(0, 0x40, 127) // op1
            .ym_write(0, 0x44, 127) // op3
            .ym_write(0, 0x48, 127) // op2
            .ym_write(0, 0x4C, 127) // op4
            // Op1 (modulator): slot 0 = reg offset 0x00
            .ym_write(0, 0x30, mod_mul & 0x0F) // MUL
            .ym_write(0, 0x40, mod_tl) // TL
            .ym_write(0, 0x50, 31) // AR=31
            .ym_write(0, 0x60, 0) // DR=0
            .ym_write(0, 0x70, 0) // SR=0
            .ym_write(0, 0x80, 0x0F) // SL=0, RR=15
            // Op2 (carrier): slot 2 = reg offset 0x08
            .ym_write(0, 0x38, car_mul & 0x0F) // MUL
            .ym_write(0, 0x48, car_tl) // TL
            .ym_write(0, 0x58, 31) // AR=31
            .ym_write(0, 0x68, 0) // DR=0
            .ym_write(0, 0x78, 0) // SR=0
            .ym_write(0, 0x88, 0x0F) // SL=0, RR=15
            // Frequency
            .ym_write(0, 0xA4, fnum_hi)
            .ym_write(0, 0xA0, fnum_lo)
            // Key-on op1 + op2: bit 4=op1, bit 5=op2 -> 0x30.
            // $28 key-on bits use operator-number order, not register-slot order.
            .ym_write(0, 0x28, 0x30)
    }

    /// Finalize and produce a `Vgm` struct.
    pub fn build(mut self) -> Vgm {
        // Ensure we have an End command
        if !self
            .commands
            .last()
            .is_some_and(|c| matches!(c, VgmCommand::End))
        {
            self.commands.push(VgmCommand::End);
        }

        let total_samples: u32 = self
            .commands
            .iter()
            .map(|c| match c {
                VgmCommand::Wait { samples } => *samples as u32,
                _ => 0,
            })
            .sum();

        Vgm {
            header: VgmHeader {
                total_samples,
                loop_offset: 0,
                loop_samples: 0,
                ym2612_clock: 7_670_453,
                sn76489_clock: 3_579_545,
                data_offset: 0,
                version: 0x150,
            },
            commands: self.commands,
            dac_data: Vec::new(),
        }
    }
}

impl Default for VgmBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ── VGM Renderer ────────────────────────────────────────────────────────

/// Renders a VGM through the genesoxide YM2612 and PSG, producing stereo f32
/// samples. The VGM timeline runs at 44100 Hz. Both chips are clocked at their
/// native rates with proper downsampling.
pub struct VgmRenderer {
    ym: Ym2612,
    psg: Psg,
    /// Output sample rate (44100 Hz for VGM).
    output_rate: f64,
    /// YM2612 native rate: master_clock / 144 (~53267 Hz).
    ym_native_rate: f64,
    /// PSG native rate: sn76489_clock / 16 (~223 kHz).
    psg_native_rate: f64,
    /// Fractional YM sample accumulator for downsampling.
    ym_phase: f64,
    /// Fractional PSG sample accumulator for downsampling.
    psg_phase: f64,
    /// PSG mix level relative to YM2612. ~22% matches hardware balance.
    psg_mix: f64,
}

impl VgmRenderer {
    pub fn new() -> Self {
        Self::with_clocks(7_670_453, 3_579_545)
    }

    pub fn with_clock(ym_clock: u32) -> Self {
        Self::with_clocks(ym_clock, 3_579_545)
    }

    pub fn with_clocks(ym_clock: u32, psg_clock: u32) -> Self {
        let ym_native_rate = if ym_clock > 0 {
            ym_clock as f64 / 144.0
        } else {
            53267.0
        };
        let psg_native_rate = if psg_clock > 0 {
            psg_clock as f64 / 16.0
        } else {
            223721.0
        };
        Self {
            ym: Ym2612::new(),
            psg: Psg::new(),
            output_rate: 44100.0,
            ym_native_rate,
            psg_native_rate,
            ym_phase: 0.0,
            psg_phase: 0.0,
            psg_mix: 0.22,
        }
    }

    /// Render the entire VGM to stereo interleaved f32 samples.
    pub fn render(&mut self, vgm: &Vgm) -> Vec<f32> {
        let capacity = (vgm.header.total_samples as usize + 1024) * 2;
        let mut output = Vec::with_capacity(capacity);
        let dac_data = &vgm.dac_data;

        for cmd in &vgm.commands {
            match *cmd {
                VgmCommand::Ym2612Port0 { reg, val } => {
                    self.ym.write_address(0, reg);
                    self.ym.write_data(0, val);
                }
                VgmCommand::Ym2612Port1 { reg, val } => {
                    self.ym.write_address(1, reg);
                    self.ym.write_data(1, val);
                }
                VgmCommand::Psg { val } => {
                    self.psg.write(val);
                }
                VgmCommand::DacWrite { offset, wait } => {
                    // Write DAC byte from data bank to register 0x2A
                    if let Some(&byte) = dac_data.get(offset as usize) {
                        self.ym.write_address(0, 0x2A);
                        self.ym.write_data(0, byte);
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

    /// Clock both chips and produce `count` output samples at 44100 Hz.
    ///
    /// Each output sample averages the YM2612 ticks that fall in that period,
    /// then adds a box-filtered PSG sample mixed at `psg_mix` level.
    fn render_samples(&mut self, count: u32, output: &mut Vec<f32>) {
        let ym_ratio = self.ym_native_rate / self.output_rate;
        let psg_ratio = self.psg_native_rate / self.output_rate;

        for _ in 0..count {
            // ── YM2612: accumulate and average ──
            self.ym_phase += ym_ratio;
            let mut left_acc: f64 = 0.0;
            let mut right_acc: f64 = 0.0;
            let mut ym_count: u32 = 0;

            while self.ym_phase >= 1.0 {
                self.ym_phase -= 1.0;
                let (l, r) = self.ym.output_sample();
                left_acc += l as f64;
                right_acc += r as f64;
                ym_count += 1;
            }

            let (ym_l, ym_r) = if ym_count > 0 {
                (left_acc / ym_count as f64, right_acc / ym_count as f64)
            } else {
                (0.0, 0.0)
            };

            // ── PSG: accumulate and average ──
            self.psg_phase += psg_ratio;
            let mut psg_acc: f64 = 0.0;
            let mut psg_count: u32 = 0;

            while self.psg_phase >= 1.0 {
                self.psg_phase -= 1.0;
                self.psg.clock_tick();
                psg_acc += self.psg.sample() as f64;
                psg_count += 1;
            }

            let psg_sample = if psg_count > 0 {
                (psg_acc / psg_count as f64) * self.psg_mix
            } else {
                0.0
            };

            // Mix: YM2612 stereo + PSG mono (both channels)
            output.push((ym_l + psg_sample) as f32);
            output.push((ym_r + psg_sample) as f32);
        }
    }

    /// Access the underlying YM2612 (for inspection in tests).
    pub fn ym(&self) -> &Ym2612 {
        &self.ym
    }

    /// Access the underlying PSG (for inspection in tests).
    pub fn psg(&self) -> &Psg {
        &self.psg
    }
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

fn trigger_ym_channel_pan_edge_persistence(
    side_memory: &[f32; 6],
    pan_edge_carry: &mut [f32; 6],
    pan_masks: &mut [u8; 6],
    amounts: [f32; 6],
    port: u8,
    addr: u8,
    value: u8,
) {
    if !(0xB4..=0xB6).contains(&addr) || port > 1 {
        return;
    }

    let channel = usize::from(addr & 0x03) + usize::from(port) * 3;
    if channel >= 6 {
        return;
    }

    let next_mask = value & 0xC0;
    if next_mask != 0xC0 {
        pan_edge_carry[channel] += side_memory[channel] * amounts[channel];
    }
    pan_masks[channel] = next_mask;
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

pub struct CoreAudioRenderer {
    ym: Ym2612,
    psg: Psg,
    output_rate: f64,
    audio_sample_phase: f64,
    ym_native_rate: f64,
    psg_native_rate: f64,
    ym_phase: f64,
    psg_phase: f64,
    audio_output_config: AudioOutputConfig,
    ym_filter_left: AudioFilterState,
    ym_filter_right: AudioFilterState,
    psg_filter: AudioFilterState,
    post_high_pass_left: AudioFilterState,
    post_high_pass_right: AudioFilterState,
    post_low_pass_left: AudioFilterState,
    post_low_pass_right: AudioFilterState,
    post_eq_1_left: AudioFilterState,
    post_eq_1_right: AudioFilterState,
    post_eq_2_left: AudioFilterState,
    post_eq_2_right: AudioFilterState,
    post_eq_3_left: AudioFilterState,
    post_eq_3_right: AudioFilterState,
    post_eq_4_left: AudioFilterState,
    post_eq_4_right: AudioFilterState,
    post_eq_5_left: AudioFilterState,
    post_eq_5_right: AudioFilterState,
    post_side_eq_1: AudioFilterState,
    post_side_eq_2: AudioFilterState,
    post_fir_left: AudioFilterState,
    post_fir_right: AudioFilterState,
    post_delay_left: SampleDelay,
    post_delay_right: SampleDelay,
    psg_mix: f32,
    ym_gain: f32,
    psg_gain: f32,
    ym_channel_side_memory_amounts: [f32; 6],
    ym_channel_side_transient_mixes: [f32; 6],
    ym_channel_side_sign_align_mixes: [f32; 6],
    ym_channel_side_decay_factors: [f32; 6],
    ym_channel_key_delay_ticks: [u64; 6],
    ym_channel_pan_edge_amounts: [f32; 6],
    ym_channel_pan_edge_decay_factors: [f32; 6],
    ym_channel_side_memory: [f32; 6],
    ym_channel_previous_side: [f32; 6],
    ym_channel_pan_edge_carry: [f32; 6],
    ym_channel_pan_masks: [u8; 6],
    pending_ym_key_writes: Vec<TimedYm2612Write>,
    stereo_crossfeed: f32,
    mid_gain: f32,
    side_gain: f32,
    master_gain: f32,
}

impl CoreAudioRenderer {
    pub fn new() -> Self {
        Self::with_clocks_and_audio_output_config(
            7_670_453,
            3_579_545,
            AudioOutputConfig::default(),
        )
    }

    pub fn with_audio_output_config(config: AudioOutputConfig) -> Self {
        Self::with_clocks_and_audio_output_config(7_670_453, 3_579_545, config)
    }

    pub fn with_clock(ym_clock: u32) -> Self {
        Self::with_clocks(ym_clock, 3_579_545)
    }

    pub fn with_clocks(ym_clock: u32, psg_clock: u32) -> Self {
        Self::with_clocks_and_audio_output_config(ym_clock, psg_clock, AudioOutputConfig::default())
    }

    pub fn with_clocks_and_audio_output_config(
        ym_clock: u32,
        psg_clock: u32,
        config: AudioOutputConfig,
    ) -> Self {
        let ym_native_rate = if ym_clock > 0 {
            ym_clock as f64 / 144.0
        } else {
            7_670_454.0 / 144.0
        };
        let psg_native_rate = if psg_clock > 0 {
            psg_clock as f64 / 16.0
        } else {
            3_579_545.0 / 16.0
        };
        let spec = config.spec_for_rates(ym_native_rate as f32, 44_100.0);
        Self {
            ym: Ym2612::new(),
            psg: Psg::new(),
            output_rate: 44_100.0,
            audio_sample_phase: 0.0,
            ym_native_rate,
            psg_native_rate,
            ym_phase: 0.0,
            psg_phase: 0.0,
            audio_output_config: config,
            ym_filter_left: AudioFilterState::from_spec(spec.ym_filter),
            ym_filter_right: AudioFilterState::from_spec(spec.ym_filter),
            psg_filter: AudioFilterState::from_spec(spec.psg_filter),
            post_high_pass_left: AudioFilterState::from_spec(spec.post_high_pass),
            post_high_pass_right: AudioFilterState::from_spec(spec.post_high_pass),
            post_low_pass_left: AudioFilterState::from_spec(spec.post_low_pass),
            post_low_pass_right: AudioFilterState::from_spec(spec.post_low_pass),
            post_eq_1_left: AudioFilterState::from_spec(spec.post_eq_1),
            post_eq_1_right: AudioFilterState::from_spec(spec.post_eq_1),
            post_eq_2_left: AudioFilterState::from_spec(spec.post_eq_2),
            post_eq_2_right: AudioFilterState::from_spec(spec.post_eq_2),
            post_eq_3_left: AudioFilterState::from_spec(spec.post_eq_3),
            post_eq_3_right: AudioFilterState::from_spec(spec.post_eq_3),
            post_eq_4_left: AudioFilterState::from_spec(spec.post_eq_4),
            post_eq_4_right: AudioFilterState::from_spec(spec.post_eq_4),
            post_eq_5_left: AudioFilterState::from_spec(spec.post_eq_5),
            post_eq_5_right: AudioFilterState::from_spec(spec.post_eq_5),
            post_side_eq_1: AudioFilterState::from_spec(spec.post_side_eq_1),
            post_side_eq_2: AudioFilterState::from_spec(spec.post_side_eq_2),
            post_fir_left: AudioFilterState::from_spec(spec.post_fir),
            post_fir_right: AudioFilterState::from_spec(spec.post_fir),
            post_delay_left: SampleDelay::new(spec.post_left_delay_samples),
            post_delay_right: SampleDelay::new(spec.post_right_delay_samples),
            psg_mix: spec.psg_mix,
            ym_gain: spec.ym_gain,
            psg_gain: spec.psg_gain,
            ym_channel_side_memory_amounts: spec.ym_channel_side_memory_amounts,
            ym_channel_side_transient_mixes: spec.ym_channel_side_transient_mixes,
            ym_channel_side_sign_align_mixes: spec.ym_channel_side_sign_align_mixes,
            ym_channel_side_decay_factors: spec.ym_channel_side_decay_factors,
            ym_channel_key_delay_ticks: spec.ym_channel_key_delay_ticks,
            ym_channel_pan_edge_amounts: spec.ym_channel_pan_edge_amounts,
            ym_channel_pan_edge_decay_factors: spec.ym_channel_pan_edge_decay_factors,
            ym_channel_side_memory: [0.0; 6],
            ym_channel_previous_side: [0.0; 6],
            ym_channel_pan_edge_carry: [0.0; 6],
            ym_channel_pan_masks: [0xC0; 6],
            pending_ym_key_writes: Vec::new(),
            stereo_crossfeed: spec.stereo_crossfeed,
            mid_gain: spec.mid_gain,
            side_gain: spec.side_gain,
            master_gain: spec.master_gain,
        }
    }

    #[must_use]
    pub fn audio_output_config(&self) -> AudioOutputConfig {
        self.audio_output_config
    }

    fn shape_mixed_sample(
        &mut self,
        filtered_left: f32,
        filtered_right: f32,
        psg_out: f32,
    ) -> (f32, f32) {
        let ym_left = filtered_left * self.ym_gain;
        let ym_right = filtered_right * self.ym_gain;
        let psg_mixed = psg_out * self.psg_mix * self.psg_gain;
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
            apply_stereo_crossfeed(shaped_left, shaped_right, self.stereo_crossfeed);
        let (mid, side) = encode_mid_side(crossfed_left, crossfed_right);
        let shaped_mid = mid * self.mid_gain;
        let shaped_side = self
            .post_side_eq_2
            .filter(self.post_side_eq_1.filter(side * self.side_gain));
        let (ms_left, ms_right) = decode_mid_side(shaped_mid, shaped_side);
        let fir_left = self.post_fir_left.filter(ms_left);
        let fir_right = self.post_fir_right.filter(ms_right);
        let delayed_left = self.post_delay_left.filter(fir_left);
        let delayed_right = self.post_delay_right.filter(fir_right);

        (
            (delayed_left * self.master_gain).clamp(-1.0, 1.0),
            (delayed_right * self.master_gain).clamp(-1.0, 1.0),
        )
    }

    /// Run an already-rendered YM-only stereo stream through the configured YM
    /// filter and the same post-mix chain used by timed/live replay.
    pub fn render_external_ym_stream(&mut self, ym_samples: &[f32]) -> Vec<f32> {
        let mut output = Vec::with_capacity(ym_samples.len());

        for frame in ym_samples.chunks_exact(2) {
            let filtered_left = self.ym_filter_left.filter(frame[0]);
            let filtered_right = self.ym_filter_right.filter(frame[1]);
            let (left, right) = self.shape_mixed_sample(filtered_left, filtered_right, 0.0);
            output.push(left);
            output.push(right);
        }

        output
    }

    /// Render the entire VGM to stereo interleaved f32 samples.
    pub fn render(&mut self, vgm: &Vgm) -> Vec<f32> {
        let capacity = (vgm.header.total_samples as usize + 1024) * 2;
        let mut output = Vec::with_capacity(capacity);
        let dac_data = &vgm.dac_data;

        for cmd in &vgm.commands {
            match *cmd {
                VgmCommand::Ym2612Port0 { reg, val } => {
                    self.ym.write_address(0, reg);
                    self.ym.write_data(0, val);
                }
                VgmCommand::Ym2612Port1 { reg, val } => {
                    self.ym.write_address(1, reg);
                    self.ym.write_data(1, val);
                }
                VgmCommand::Psg { val } => {
                    self.psg.write(val);
                }
                VgmCommand::DacWrite { offset, wait } => {
                    if let Some(&byte) = dac_data.get(offset as usize) {
                        self.ym.write_address(0, 0x2A);
                        self.ym.write_data(0, byte);
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

    /// Render writes in the same scanline-batched order as GenesisCore::step_frame.
    pub fn render_scanline_batched_writes(
        &mut self,
        ym_writes: &[TimedYm2612Write],
        psg_writes: &[TimedPsgWrite],
        start_scanline: u64,
        scanline_count: u64,
    ) -> Vec<f32> {
        fn ym_scanline(write: &TimedYm2612Write) -> u64 {
            write.frame * 262 + u64::from(write.scanline)
        }

        fn psg_scanline(write: &TimedPsgWrite) -> u64 {
            write.frame * 262 + u64::from(write.scanline)
        }

        let mut output = Vec::new();
        let end_scanline = start_scanline + scanline_count;
        let mut ym_idx = ym_writes.partition_point(|write| ym_scanline(write) < start_scanline);
        let mut psg_idx = psg_writes.partition_point(|write| psg_scanline(write) < start_scanline);

        for scanline in start_scanline..end_scanline {
            let scanline_end_tick = (scanline + 1) * 3420;
            while self
                .pending_ym_key_writes
                .first()
                .is_some_and(|write| write.master_tick < scanline_end_tick)
            {
                let write = self.pending_ym_key_writes.remove(0);
                self.ym.write_address(write.port, write.addr);
                self.ym.write_data(write.port, write.value);
            }

            while let Some(write) = ym_writes.get(ym_idx).copied() {
                if ym_scanline(&write) != scanline {
                    break;
                }
                if !maybe_delay_ym_key_write(
                    &mut self.pending_ym_key_writes,
                    self.ym_channel_key_delay_ticks,
                    write,
                ) {
                    self.ym.write_address(write.port, write.addr);
                    trigger_ym_channel_pan_edge_persistence(
                        &self.ym_channel_side_memory,
                        &mut self.ym_channel_pan_edge_carry,
                        &mut self.ym_channel_pan_masks,
                        self.ym_channel_pan_edge_amounts,
                        write.port,
                        write.addr,
                        write.value,
                    );
                    self.ym.write_data(write.port, write.value);
                }
                ym_idx += 1;
            }

            while let Some(write) = psg_writes.get(psg_idx).copied() {
                if psg_scanline(&write) != scanline {
                    break;
                }
                self.psg.write(write.value);
                psg_idx += 1;
            }

            self.audio_sample_phase += self.output_rate / (262.0 * 59.92);
            while self.audio_sample_phase >= 1.0 {
                self.audio_sample_phase -= 1.0;
                self.render_samples(1, &mut output);
            }
        }

        output
    }

    /// Render timed live writes directly without quantizing them into VGM waits.
    ///
    /// This preserves sub-sample write timing, which matters when comparing
    /// against the core's live audio capture path.
    ///
    /// The renderer must already be in the chip/filter state corresponding to
    /// `start_tick`. Fresh renderers therefore only produce exact results when
    /// replay begins at the audio origin (tick 0) or after rendering the full
    /// prelude leading into `start_tick`.
    pub fn render_timed_writes(
        &mut self,
        ym_writes: &[TimedYm2612Write],
        psg_writes: &[TimedPsgWrite],
        start_tick: u64,
        end_tick: u64,
    ) -> Vec<f32> {
        const YM_TICKS_PER_SAMPLE: u64 = 1008;
        const PSG_TICKS_PER_SAMPLE: u64 = 240;

        let total_samples = samples_from_master_ticks(end_tick.saturating_sub(start_tick));
        let mut output = Vec::with_capacity(total_samples as usize * 2);

        let mut ym_idx = ym_writes.partition_point(|write| write.master_tick < start_tick);
        let mut psg_idx = psg_writes.partition_point(|write| write.master_tick < start_tick);

        let ym_phase = start_tick % YM_TICKS_PER_SAMPLE;
        let psg_phase = start_tick % PSG_TICKS_PER_SAMPLE;
        let mut next_ym_tick = start_tick
            + if ym_phase == 0 {
                YM_TICKS_PER_SAMPLE
            } else {
                YM_TICKS_PER_SAMPLE - ym_phase
            };
        let mut next_psg_tick = start_tick
            + if psg_phase == 0 {
                PSG_TICKS_PER_SAMPLE
            } else {
                PSG_TICKS_PER_SAMPLE - psg_phase
            };

        for sample_idx in 0..total_samples {
            let window_end = start_tick + master_ticks_from_output_samples(sample_idx + 1);
            let mut ym_left_acc = 0.0f64;
            let mut ym_right_acc = 0.0f64;
            let mut ym_count = 0u32;
            let mut psg_acc = 0.0f64;
            let mut psg_count = 0u32;

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
                    .pending_ym_key_writes
                    .first()
                    .map(|write| write.master_tick);
                let next_chip_tick = next_ym_tick.min(next_psg_tick);

                if let Some(write_tick) = next_delayed_key_tick {
                    if write_tick < window_end
                        && write_tick <= next_chip_tick
                        && next_write_tick.is_none_or(|tick| write_tick <= tick)
                    {
                        let write = self.pending_ym_key_writes.remove(0);
                        self.ym.write_address(write.port, write.addr);
                        self.ym.write_data(write.port, write.value);
                        continue;
                    }
                }

                if let Some(write_tick) = next_write_tick {
                    if write_tick < window_end
                        && write_tick <= next_chip_tick
                        && next_delayed_key_tick.is_none_or(|tick| write_tick < tick)
                    {
                        if next_write_is_ym {
                            let write = ym_writes[ym_idx];
                            if !maybe_delay_ym_key_write(
                                &mut self.pending_ym_key_writes,
                                self.ym_channel_key_delay_ticks,
                                write,
                            ) {
                                self.ym.write_address(write.port, write.addr);
                                trigger_ym_channel_pan_edge_persistence(
                                    &self.ym_channel_side_memory,
                                    &mut self.ym_channel_pan_edge_carry,
                                    &mut self.ym_channel_pan_masks,
                                    self.ym_channel_pan_edge_amounts,
                                    write.port,
                                    write.addr,
                                    write.value,
                                );
                                self.ym.write_data(write.port, write.value);
                            }
                            ym_idx += 1;
                        } else {
                            self.psg.write(psg_writes[psg_idx].value);
                            psg_idx += 1;
                        }
                        continue;
                    }
                }

                if next_ym_tick < window_end && next_ym_tick <= next_psg_tick {
                    let channel_samples = self.ym.output_sample_per_channel();
                    let (left, right) = mix_ym_channel_outputs_with_side_memory(
                        channel_samples,
                        &mut self.ym_channel_side_memory,
                        self.ym_channel_side_memory_amounts,
                        self.ym_channel_side_transient_mixes,
                        self.ym_channel_side_sign_align_mixes,
                        self.ym_channel_side_decay_factors,
                        &mut self.ym_channel_previous_side,
                        &mut self.ym_channel_pan_edge_carry,
                        self.ym_channel_pan_edge_decay_factors,
                    );
                    ym_left_acc += f64::from(self.ym_filter_left.filter(left));
                    ym_right_acc += f64::from(self.ym_filter_right.filter(right));
                    ym_count += 1;
                    next_ym_tick += YM_TICKS_PER_SAMPLE;
                    continue;
                }

                if next_psg_tick < window_end {
                    self.psg.clock_tick();
                    psg_acc += f64::from(self.psg_filter.filter(self.psg.sample()));
                    psg_count += 1;
                    next_psg_tick += PSG_TICKS_PER_SAMPLE;
                    continue;
                }

                break;
            }

            let filtered_left = if ym_count > 0 {
                (ym_left_acc / ym_count as f64) as f32
            } else {
                self.ym_filter_left.last_output()
            };
            let filtered_right = if ym_count > 0 {
                (ym_right_acc / ym_count as f64) as f32
            } else {
                self.ym_filter_right.last_output()
            };
            let psg_out = if psg_count > 0 {
                (psg_acc / psg_count as f64) as f32
            } else {
                self.psg_filter.last_output()
            };
            let (left, right) = self.shape_mixed_sample(filtered_left, filtered_right, psg_out);
            output.push(left);
            output.push(right);
        }

        output
    }

    fn render_samples(&mut self, count: u32, output: &mut Vec<f32>) {
        let ym_ratio = self.ym_native_rate / self.output_rate;
        let psg_ratio = self.psg_native_rate / self.output_rate;

        for _ in 0..count {
            self.ym_phase += ym_ratio;
            let mut ym_left_acc = 0.0f64;
            let mut ym_right_acc = 0.0f64;
            let mut ym_count = 0u32;
            while self.ym_phase >= 1.0 {
                self.ym_phase -= 1.0;
                let channel_samples = self.ym.output_sample_per_channel();
                let (left, right) = mix_ym_channel_outputs_with_side_memory(
                    channel_samples,
                    &mut self.ym_channel_side_memory,
                    self.ym_channel_side_memory_amounts,
                    self.ym_channel_side_transient_mixes,
                    self.ym_channel_side_sign_align_mixes,
                    self.ym_channel_side_decay_factors,
                    &mut self.ym_channel_previous_side,
                    &mut self.ym_channel_pan_edge_carry,
                    self.ym_channel_pan_edge_decay_factors,
                );
                ym_left_acc += f64::from(self.ym_filter_left.filter(left));
                ym_right_acc += f64::from(self.ym_filter_right.filter(right));
                ym_count += 1;
            }

            let filtered_left = if ym_count > 0 {
                (ym_left_acc / ym_count as f64) as f32
            } else {
                self.ym_filter_left.last_output()
            };
            let filtered_right = if ym_count > 0 {
                (ym_right_acc / ym_count as f64) as f32
            } else {
                self.ym_filter_right.last_output()
            };

            self.psg_phase += psg_ratio;
            let mut psg_acc = 0.0f64;
            let mut psg_count = 0u32;
            while self.psg_phase >= 1.0 {
                self.psg_phase -= 1.0;
                self.psg.clock_tick();
                psg_acc += f64::from(self.psg_filter.filter(self.psg.sample()));
                psg_count += 1;
            }
            let psg_out = if psg_count > 0 {
                (psg_acc / psg_count as f64) as f32
            } else {
                self.psg_filter.last_output()
            };
            let (left, right) = self.shape_mixed_sample(filtered_left, filtered_right, psg_out);
            output.push(left);
            output.push(right);
        }
    }
}

impl Default for VgmRenderer {
    fn default() -> Self {
        Self::new()
    }
}

// ── Audio analysis utilities ────────────────────────────────────────────

/// Compute RMS energy of a slice of samples.
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum_sq / samples.len() as f64).sqrt() as f32
}

/// Count zero crossings in a mono signal. Useful for estimating frequency.
pub fn zero_crossings(samples: &[f32]) -> u32 {
    samples
        .windows(2)
        .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
        .count() as u32
}

/// Estimate fundamental frequency from zero crossings.
/// `sample_rate` is typically 44100.
pub fn estimate_frequency(mono_samples: &[f32], sample_rate: u32) -> f32 {
    let crossings = zero_crossings(mono_samples);
    if mono_samples.len() < 2 {
        return 0.0;
    }
    let duration = (mono_samples.len() - 1) as f32 / sample_rate as f32;
    // Each full cycle has 2 zero crossings
    crossings as f32 / (2.0 * duration)
}

/// Extract the left channel from stereo interleaved samples.
pub fn left_channel(stereo: &[f32]) -> Vec<f32> {
    stereo.iter().step_by(2).copied().collect()
}

/// Extract the right channel from stereo interleaved samples.
pub fn right_channel(stereo: &[f32]) -> Vec<f32> {
    stereo.iter().skip(1).step_by(2).copied().collect()
}

/// Peak absolute amplitude.
pub fn peak(samples: &[f32]) -> f32 {
    samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max)
}

/// Cross-correlation between two signals (Pearson coefficient, -1.0 to 1.0).
pub fn cross_correlation(a: &[f32], b: &[f32]) -> f32 {
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

/// Save stereo interleaved f32 samples to a 16-bit WAV file.
pub fn save_wav(path: &Path, samples: &[f32], sample_rate: u32) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer =
        hound::WavWriter::create(path, spec).map_err(|e| format!("WAV create failed: {e}"))?;
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        writer
            .write_sample((clamped * 32767.0) as i16)
            .map_err(|e| format!("WAV write failed: {e}"))?;
    }
    writer
        .finalize()
        .map_err(|e| format!("WAV finalize failed: {e}"))?;
    Ok(())
}

/// Load a WAV file as stereo interleaved f32 samples.
pub fn load_wav(path: &Path) -> Result<(u32, Vec<f32>), String> {
    let reader = hound::WavReader::open(path).map_err(|e| format!("WAV open failed: {e}"))?;
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
    Ok((rate, samples))
}

#[cfg(test)]
mod tests {
    use super::*;
    use genesoxide_core::api::TimedPsgWrite;
    use genesoxide_core::api::TimedYm2612Write;
    use genesoxide_core::scheduler::MASTER_CLOCK_NTSC;

    #[test]
    fn builder_produces_valid_vgm() {
        let vgm = VgmBuilder::new().ym_write(0, 0xB0, 0x07).wait(100).build();

        assert_eq!(vgm.header.total_samples, 100);
        assert_eq!(vgm.header.ym2612_clock, 7_670_453);
        assert!(matches!(vgm.commands.last(), Some(VgmCommand::End)));
    }

    #[test]
    fn renderer_produces_correct_sample_count() {
        let vgm = VgmBuilder::new().wait(4410).build(); // 0.1 seconds

        let mut renderer = VgmRenderer::new();
        let samples = renderer.render(&vgm);

        // 4410 output samples × 2 channels = 8820
        assert_eq!(samples.len(), 8820);
    }

    #[test]
    fn core_audio_renderer_produces_correct_sample_count() {
        let vgm = VgmBuilder::new().wait(4410).build(); // 0.1 seconds

        let mut renderer = CoreAudioRenderer::new();
        let samples = renderer.render(&vgm);

        assert_eq!(samples.len(), 8820);
    }

    #[test]
    fn core_audio_renderer_accepts_explicit_output_config() {
        let config = genesoxide_core::api::AudioOutputConfig::new(
            genesoxide_core::api::AudioOutputProfile::Legacy,
            2.0,
        )
        .with_ym_gain(1.5)
        .with_psg_gain(0.75)
        .with_ym_channel_side_memory_amounts([0.20, 0.0, 0.0, 0.15, 0.0, 0.0])
        .with_ym_channel_side_decay_ms([0.0, 0.0, 0.0, 12.0, 25.0, 0.0])
        .with_ym_channel_key_delay_ms([5.0, 0.0, 0.0, 5.0, 5.0, 0.0])
        .with_ym_channel_pan_edge_amounts([0.0, 0.0, 0.0, 0.08, 0.12, 0.0])
        .with_ym_channel_pan_edge_decay_ms([0.0, 0.0, 0.0, 12.0, 25.0, 0.0])
        .with_stereo_crossfeed(0.10)
        .with_mid_gain(1.01)
        .with_side_gain(0.97)
        .with_post_high_pass_hz(60.0)
        .with_post_low_pass_hz(12_000.0)
        .with_post_eq_1(genesoxide_core::api::AudioEqStage::low_shelf(110.0, -8.0))
        .with_post_eq_2(genesoxide_core::api::AudioEqStage::peaking(
            420.0, 0.75, 6.0,
        ))
        .with_post_eq_3(genesoxide_core::api::AudioEqStage::high_shelf(
            2_600.0, -3.5,
        ))
        .with_post_eq_4(genesoxide_core::api::AudioEqStage::peaking(
            190.0, 0.90, 3.0,
        ))
        .with_post_eq_5(genesoxide_core::api::AudioEqStage::peaking(
            760.0, 1.10, 1.5,
        ))
        .with_post_side_eq_1(genesoxide_core::api::AudioEqStage::peaking(
            420.0, 0.90, -1.5,
        ))
        .with_post_side_eq_2(genesoxide_core::api::AudioEqStage::peaking(
            900.0, 1.00, 1.2,
        ))
        .with_post_fir_taps([0.88, 0.10, 0.02, 0.0, 0.0])
        .with_post_left_delay_samples(1)
        .with_post_right_delay_samples(2);
        let renderer = CoreAudioRenderer::with_audio_output_config(config);

        assert_eq!(renderer.audio_output_config(), config);
    }

    #[test]
    fn centered_single_op_tone_stays_mono_under_neutral_legacy_output() {
        let vgm = VgmBuilder::new()
            .single_op_tone(653, 4, 0, 31)
            .wait(4410)
            .build();
        let config = genesoxide_core::api::AudioOutputConfig::legacy().with_psg_gain(0.0);
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let samples = renderer.render(&vgm);

        let left = left_channel(&samples);
        let right = right_channel(&samples);
        let diff: Vec<f32> = left.iter().zip(&right).map(|(&l, &r)| l - r).collect();
        let signal_rms = rms(&left[200..]).max(1e-9);
        let side_rms = rms(&diff[200..]);

        assert!(
            side_rms < signal_rms * 0.01,
            "expected centered pan to stay mono, got side_rms={side_rms:.6} signal_rms={signal_rms:.6}"
        );
    }

    #[test]
    fn timed_centered_single_op_tone_stays_mono_under_neutral_legacy_output() {
        let end_tick = master_ticks_from_output_samples(4410);
        let writes = [
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
                value: 0xC0,
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
        ];
        let config = genesoxide_core::api::AudioOutputConfig::legacy().with_psg_gain(0.0);
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let samples = renderer.render_timed_writes(&writes, &[], 0, end_tick);

        let left = left_channel(&samples);
        let right = right_channel(&samples);
        let diff: Vec<f32> = left.iter().zip(&right).map(|(&l, &r)| l - r).collect();
        let signal_rms = rms(&left[200..]).max(1e-9);
        let side_rms = rms(&diff[200..]);

        assert!(
            side_rms < signal_rms * 0.01,
            "expected timed centered pan to stay mono, got side_rms={side_rms:.6} signal_rms={signal_rms:.6}"
        );
    }

    #[test]
    fn timed_centered_single_op_tone_stays_mono_with_ym_side_memory_config() {
        let end_tick = master_ticks_from_output_samples(4410);
        let writes = [
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
                value: 0xC0,
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
        ];
        let config = genesoxide_core::api::AudioOutputConfig::legacy()
            .with_psg_gain(0.0)
            .with_ym_channel_side_memory_amounts([0.20, 0.0, 0.0, 0.15, 0.0, 0.0])
            .with_ym_channel_side_decay_ms([0.0, 0.0, 0.0, 12.0, 25.0, 0.0]);
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let samples = renderer.render_timed_writes(&writes, &[], 0, end_tick);

        let left = left_channel(&samples);
        let right = right_channel(&samples);
        let diff: Vec<f32> = left.iter().zip(&right).map(|(&l, &r)| l - r).collect();
        let signal_rms = rms(&left[200..]).max(1e-9);
        let side_rms = rms(&diff[200..]);

        assert!(
            side_rms < signal_rms * 0.01,
            "expected timed centered pan to stay mono with side-memory config, got side_rms={side_rms:.6} signal_rms={signal_rms:.6}"
        );
    }

    #[test]
    fn timed_centered_single_op_tone_stays_mono_with_ym_side_decay_config() {
        let end_tick = master_ticks_from_output_samples(4410);
        let writes = [
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
                value: 0xC0,
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
        ];
        let config = genesoxide_core::api::AudioOutputConfig::legacy()
            .with_psg_gain(0.0)
            .with_ym_channel_side_memory_amounts([0.20, 0.0, 0.0, 0.15, 0.0, 0.0])
            .with_ym_channel_side_decay_ms([20.0, 0.0, 0.0, 12.0, 25.0, 0.0]);
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let samples = renderer.render_timed_writes(&writes, &[], 0, end_tick);

        let left = left_channel(&samples);
        let right = right_channel(&samples);
        let diff: Vec<f32> = left.iter().zip(&right).map(|(&l, &r)| l - r).collect();
        let signal_rms = rms(&left[200..]).max(1e-9);
        let side_rms = rms(&diff[200..]);

        assert!(
            side_rms < signal_rms * 0.01,
            "expected timed centered pan to stay mono with side-decay config, got side_rms={side_rms:.6} signal_rms={signal_rms:.6}"
        );
    }

    #[test]
    fn timed_centered_single_op_tone_stays_mono_with_ym_side_transient_mix_config() {
        let end_tick = master_ticks_from_output_samples(4410);
        let writes = [
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
                value: 0xC0,
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
                value: 20,
            },
            TimedYm2612Write {
                master_tick: 0,
                frame: 0,
                scanline: 0,
                port: 0,
                addr: 0x70,
                value: 0x0F,
            },
            TimedYm2612Write {
                master_tick: 0,
                frame: 0,
                scanline: 0,
                port: 0,
                addr: 0x80,
                value: 0x1F,
            },
            TimedYm2612Write {
                master_tick: 0,
                frame: 0,
                scanline: 0,
                port: 0,
                addr: 0x90,
                value: 0x00,
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
        ];
        let config = genesoxide_core::api::AudioOutputConfig::legacy()
            .with_psg_gain(0.0)
            .with_ym_channel_side_memory_amounts([0.20, 0.0, 0.0, 0.60, 0.0, 0.0])
            .with_ym_channel_side_transient_mixes([1.0, 0.0, 0.0, 1.0, 0.0, 0.0])
            .with_ym_channel_side_decay_ms([0.01, 0.0, 0.0, 0.02, 0.0, 0.0]);
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let samples = renderer.render_timed_writes(&writes, &[], 0, end_tick);

        let left = left_channel(&samples);
        let right = right_channel(&samples);
        let diff: Vec<f32> = left.iter().zip(&right).map(|(&l, &r)| l - r).collect();
        let signal_rms = rms(&left[200..]).max(1e-9);
        let side_rms = rms(&diff[200..]);

        assert!(
            side_rms < signal_rms * 0.01,
            "expected timed centered pan to stay mono with side-transient config, got side_rms={side_rms:.6} signal_rms={signal_rms:.6}"
        );
    }

    #[test]
    fn timed_centered_single_op_tone_stays_mono_with_ym_pan_edge_persistence_config() {
        let end_tick = master_ticks_from_output_samples(4410);
        let writes = [
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
                value: 0xC0,
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
        ];
        let config = genesoxide_core::api::AudioOutputConfig::legacy()
            .with_psg_gain(0.0)
            .with_ym_channel_pan_edge_amounts([0.20, 0.0, 0.0, 0.15, 0.0, 0.0])
            .with_ym_channel_pan_edge_decay_ms([25.0, 0.0, 0.0, 10.0, 0.0, 0.0]);
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let samples = renderer.render_timed_writes(&writes, &[], 0, end_tick);

        let left = left_channel(&samples);
        let right = right_channel(&samples);
        let diff: Vec<f32> = left.iter().zip(&right).map(|(&l, &r)| l - r).collect();
        let signal_rms = rms(&left[200..]).max(1e-9);
        let side_rms = rms(&diff[200..]);

        assert!(
            side_rms < signal_rms * 0.01,
            "expected timed centered pan to stay mono with pan-edge persistence config, got side_rms={side_rms:.6} signal_rms={signal_rms:.6}"
        );
    }

    #[test]
    fn timed_ym_key_delay_defers_selected_channel_key_on() {
        let end_tick = master_ticks_from_output_samples(2205);
        let writes = [
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
                value: 0xC0,
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
        ];
        let config = genesoxide_core::api::AudioOutputConfig::legacy()
            .with_psg_gain(0.0)
            .with_ym_channel_key_delay_ms([15.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let samples = renderer.render_timed_writes(&writes, &[], 0, end_tick);

        let left = left_channel(&samples);
        let early_rms = rms(&left[..300]);
        let late_rms = rms(&left[1100..1600]);

        assert!(
            early_rms < late_rms * 0.10,
            "expected YM key delay to defer onset, got early_rms={early_rms:.6} late_rms={late_rms:.6}"
        );
    }

    #[test]
    fn silent_vgm_produces_silence() {
        let vgm = VgmBuilder::new().wait(1000).build();

        let mut renderer = VgmRenderer::new();
        let samples = renderer.render(&vgm);
        let peak = peak(&samples);

        assert!(
            peak < 0.001,
            "Silent VGM should produce near-silence, got peak {peak}"
        );
    }

    #[test]
    fn timed_ym2612_trace_converts_into_waited_vgm_commands() {
        let ten_samples =
            (u64::from(10u32) * MASTER_CLOCK_NTSC + u64::from(44_100u32 / 2)) / 44_100;
        let twenty_samples =
            (u64::from(20u32) * MASTER_CLOCK_NTSC + u64::from(44_100u32 / 2)) / 44_100;

        let writes = [
            TimedYm2612Write {
                master_tick: ten_samples,
                frame: 0,
                scanline: 0,
                port: 0,
                addr: 0x28,
                value: 0x30,
            },
            TimedYm2612Write {
                master_tick: twenty_samples,
                frame: 0,
                scanline: 0,
                port: 1,
                addr: 0xB6,
                value: 0xC0,
            },
        ];

        let vgm = vgm_from_timed_ym2612_writes(&writes, 0, twenty_samples + ten_samples);

        assert_eq!(vgm.header.total_samples, 30);
        assert_eq!(
            vgm.commands,
            vec![
                VgmCommand::Wait { samples: 10 },
                VgmCommand::Ym2612Port0 {
                    reg: 0x28,
                    val: 0x30,
                },
                VgmCommand::Wait { samples: 10 },
                VgmCommand::Ym2612Port1 {
                    reg: 0xB6,
                    val: 0xC0,
                },
                VgmCommand::Wait { samples: 10 },
                VgmCommand::End,
            ]
        );
    }

    #[test]
    fn soundlog_extracts_key_on_tone_change_and_key_off_from_timed_ym_stream() {
        let ten_samples =
            (u64::from(10u32) * MASTER_CLOCK_NTSC + u64::from(44_100u32 / 2)) / 44_100;
        let twenty_samples =
            (u64::from(20u32) * MASTER_CLOCK_NTSC + u64::from(44_100u32 / 2)) / 44_100;
        let thirty_samples =
            (u64::from(30u32) * MASTER_CLOCK_NTSC + u64::from(44_100u32 / 2)) / 44_100;
        let forty_samples =
            (u64::from(40u32) * MASTER_CLOCK_NTSC + u64::from(44_100u32 / 2)) / 44_100;

        let writes = [
            TimedYm2612Write {
                master_tick: 0,
                frame: 0,
                scanline: 0,
                port: 0,
                addr: 0xB4,
                value: 0xC0,
            },
            TimedYm2612Write {
                master_tick: 0,
                frame: 0,
                scanline: 0,
                port: 0,
                addr: 0xA4,
                value: 0x22,
            },
            TimedYm2612Write {
                master_tick: 0,
                frame: 0,
                scanline: 0,
                port: 0,
                addr: 0xA0,
                value: 0x69,
            },
            TimedYm2612Write {
                master_tick: ten_samples,
                frame: 0,
                scanline: 0,
                port: 0,
                addr: 0x28,
                value: 0xF0,
            },
            TimedYm2612Write {
                master_tick: twenty_samples,
                frame: 0,
                scanline: 0,
                port: 0,
                addr: 0xA4,
                value: 0x24,
            },
            TimedYm2612Write {
                master_tick: twenty_samples,
                frame: 0,
                scanline: 0,
                port: 0,
                addr: 0xA0,
                value: 0x34,
            },
            TimedYm2612Write {
                master_tick: thirty_samples,
                frame: 0,
                scanline: 0,
                port: 0,
                addr: 0x28,
                value: 0x00,
            },
        ];

        let events =
            extract_ym2612_state_events_from_timed_writes(&writes, 0, forty_samples).unwrap();

        assert_eq!(events.len(), 3, "expected KeyOn, ToneChange, and KeyOff");
        assert_eq!(events[0].sample, 10);
        assert_eq!(events[0].channel, 0);
        assert_eq!(events[0].kind, Ym2612TrackedEventKind::KeyOn);
        assert!(events[0].freq_hz.is_some());

        assert_eq!(events[1].sample, 20);
        assert_eq!(events[1].channel, 0);
        assert_eq!(events[1].kind, Ym2612TrackedEventKind::ToneChange);
        assert!(events[1].freq_hz.is_some());

        assert_eq!(events[2].sample, 30);
        assert_eq!(events[2].channel, 0);
        assert_eq!(events[2].kind, Ym2612TrackedEventKind::KeyOff);
        assert_eq!(events[2].freq_hz, None);
    }

    #[test]
    fn timed_sound_writes_convert_into_mixed_vgm_commands() {
        let ten_samples =
            (u64::from(10u32) * MASTER_CLOCK_NTSC + u64::from(44_100u32 / 2)) / 44_100;
        let fifteen_samples =
            (u64::from(15u32) * MASTER_CLOCK_NTSC + u64::from(44_100u32 / 2)) / 44_100;
        let twenty_samples =
            (u64::from(20u32) * MASTER_CLOCK_NTSC + u64::from(44_100u32 / 2)) / 44_100;

        let ym_writes = [TimedYm2612Write {
            master_tick: fifteen_samples,
            frame: 0,
            scanline: 0,
            port: 0,
            addr: 0x28,
            value: 0x30,
        }];
        let psg_writes = [TimedPsgWrite {
            master_tick: ten_samples,
            frame: 0,
            scanline: 0,
            value: 0x9F,
        }];

        let vgm = vgm_from_timed_sound_writes(&ym_writes, &psg_writes, 0, twenty_samples);

        assert_eq!(vgm.header.total_samples, 20);
        assert_eq!(
            vgm.commands,
            vec![
                VgmCommand::Wait { samples: 10 },
                VgmCommand::Psg { val: 0x9F },
                VgmCommand::Wait { samples: 5 },
                VgmCommand::Ym2612Port0 {
                    reg: 0x28,
                    val: 0x30,
                },
                VgmCommand::Wait { samples: 5 },
                VgmCommand::End,
            ]
        );
    }

    #[test]
    fn external_ym_stream_preserves_sample_count() {
        let input: Vec<f32> = (0..256)
            .flat_map(|idx| {
                let t = idx as f32 / 44_100.0;
                let left = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.25;
                let right = (2.0 * std::f32::consts::PI * 660.0 * t).sin() * 0.20;
                [left, right]
            })
            .collect();

        let mut renderer = CoreAudioRenderer::with_audio_output_config(
            genesoxide_core::api::AudioOutputConfig::default(),
        );
        let output = renderer.render_external_ym_stream(&input);

        assert_eq!(output.len(), input.len());
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn external_ym_stream_respects_zero_ym_gain() {
        let input: Vec<f32> = (0..64)
            .flat_map(|idx| {
                let t = idx as f32 / 44_100.0;
                let left = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.50;
                let right = (2.0 * std::f32::consts::PI * 554.37 * t).sin() * 0.40;
                [left, right]
            })
            .collect();

        let config = genesoxide_core::api::AudioOutputConfig::default().with_ym_gain(0.0);
        let mut renderer = CoreAudioRenderer::with_audio_output_config(config);
        let output = renderer.render_external_ym_stream(&input);

        assert!(
            output.iter().all(|sample| sample.abs() < 1e-6),
            "expected zero YM gain to mute external YM stream, got peak {}",
            peak(&output)
        );
    }
}

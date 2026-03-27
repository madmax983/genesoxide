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
use crate::rom::{self, RomHeader};
use crate::scheduler::Scheduler;
use crate::vdp::Vdp;
use crate::ym2612;
use crate::z80;

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

/// Genesis controller button (re-exported from io module).
pub use crate::io::Button;

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
    /// Pause emulation.
    Pause,
    /// Resume emulation.
    Resume,
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
    /// Accumulated audio samples for the current frame (stereo interleaved f32).
    audio_buffer: Vec<f32>,
    /// Fractional audio sample accumulator for sub-scanline sample timing.
    audio_sample_phase: f64,
    /// Output audio sample rate in Hz (default 44100).
    audio_sample_rate: f64,
    /// Frame counter.
    frame_count: u64,
    /// Emulation speed in permille.
    speed_permille: u16,
    /// Whether emulation is paused.
    paused: bool,
}

impl GenesisCore {
    /// Creates a new Genesis core with no ROM loaded.
    #[must_use]
    pub fn new() -> Self {
        Self {
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
            psg: psg::Psg::new(),
            ym2612: ym2612::Ym2612::new(),
            audio_buffer: Vec::with_capacity(1600),
            audio_sample_phase: 0.0,
            audio_sample_rate: 44100.0,
            frame_count: 0,
            speed_permille: 1000,
            paused: false,
        }
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
            Command::SetAudioSampleRate(rate) => self.audio_sample_rate = f64::from(rate),
            Command::Pause => self.paused = true,
            Command::Resume => self.paused = false,
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

    /// Returns the Z80 RAM (8KB) for debugging.
    #[must_use]
    pub fn z80_ram(&self) -> &[u8] {
        &*self.z80_ram
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
        self.psg = psg::Psg::new();
        self.ym2612 = ym2612::Ym2612::new();
        self.audio_buffer.clear();
        self.audio_sample_phase = 0.0;
        self.frame_count = 0;

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

    fn step_cpu(&mut self) {
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
        };

        let cycles = cpu::execute_instruction(&mut self.cpu, opcode, &mut bus);
        let cycles_u64 = u64::from(cycles);
        self.cpu.cycles += cycles_u64;
        self.scheduler.advance_cpu(cycles_u64);
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

    fn step_frame(&mut self) {
        if self.paused {
            return;
        }

        // Clear audio buffer at frame start
        self.audio_buffer.clear();

        // Clear V-blank at frame start
        self.vdp.set_vblank(false);

        for scanline in 0..SCANLINES_PER_FRAME {
            // Begin scanline timing
            self.vdp.begin_scanline(scanline);

            // Reset per-scanline bus tracking before the 68K runs.
            self.z80_bus_released_this_scanline = false;

            // Run CPU for this scanline
            self.step_scanline();

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
                self.step_z80_scanline();
            }

            // Check for H-interrupt (level 4)
            if self.vdp.h_interrupt_pending() {
                self.vdp.clear_h_interrupt();
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

                // Fire level 6 interrupt if V-interrupt is enabled (reg 1, bit 5)
                let vint_enabled = self.vdp.read_register(1) & 0x20 != 0;
                if vint_enabled {
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
                    };
                    let cycles = cpu::deliver_interrupt(&mut self.cpu, &mut bus, 6);
                    self.cpu.cycles += u64::from(cycles);
                    self.scheduler.advance_cpu(u64::from(cycles));
                }
            }

            // Collect audio samples for this scanline
            self.collect_audio_samples();
        }

        self.vdp.end_frame();
        self.port1.reset_th_counter();
        self.port2.reset_th_counter();
        self.frame_count += 1;
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

    /// Steps the Z80 for one scanline (~228 T-states).
    fn step_z80_scanline(&mut self) {
        // Z80 @ master/15 = ~3.58 MHz. Per scanline = 3416 master clocks / 15 ~ 228 T-states
        let target = self.z80.cycles + 228;
        while self.z80.cycles < target {
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
            };
            let cycles = z80::execute_instruction(&mut self.z80, &mut bus);
            self.z80.cycles += u64::from(cycles);
        }
    }

    /// Collects audio samples for one scanline.
    ///
    /// Called at the end of each scanline in `step_frame`. Generates
    /// approximately 2.81 stereo sample pairs per scanline, yielding
    /// ~736 pairs per frame at 44100 Hz.
    fn collect_audio_samples(&mut self) {
        // samples_per_scanline = sample_rate / (262 lines * 59.92 fps)
        self.audio_sample_phase += self.audio_sample_rate / (262.0 * 59.92);

        while self.audio_sample_phase >= 1.0 {
            self.audio_sample_phase -= 1.0;

            // Clock PSG: 3_579_545 Hz / sample_rate ticks per output sample
            let psg_ticks = (3_579_545.0 / self.audio_sample_rate).round() as u32;
            for _ in 0..psg_ticks {
                self.psg.clock_tick();
            }

            // Get YM2612 output (also advances timers internally)
            let (ym_l, ym_r) = self.ym2612.output_sample();

            // Get PSG output
            let psg_out = self.psg.sample();

            // Mix: YM2612 stereo + PSG mono (into both channels)
            let left = (ym_l + psg_out * 0.5).clamp(-1.0, 1.0);
            let right = (ym_r + psg_out * 0.5).clamp(-1.0, 1.0);
            self.audio_buffer.push(left);
            self.audio_buffer.push(right);
        }
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
                    0x0000..=0x1FFF => self.z80_ram[z80_addr as usize] = val,
                    0x2000..=0x3FFF => self.z80_ram[(z80_addr & 0x1FFF) as usize] = val,
                    0x4000 => self.ym2612.write_address(0, val),
                    0x4001 => self.ym2612.write_data(0, val),
                    0x4002 => self.ym2612.write_address(1, val),
                    0x4003 => self.ym2612.write_data(1, val),
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
                        self.psg.write(val);
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
                        self.ym2612.write_data(0, lo);
                    }
                    0x4002 => {
                        self.ym2612.write_address(1, hi);
                        self.ym2612.write_data(1, lo);
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
            0x4001 => self.ym2612.write_data(0, val),
            0x4002 => self.ym2612.write_address(1, val),
            0x4003 => self.ym2612.write_data(1, val),
            0x6000..=0x60FF => {
                // Bank register: shift in one bit at a time (bit 0 of val),
                // 9 bits forming bits 15-23 of the ROM address.
                *self.z80_bank = ((*self.z80_bank >> 1) | ((u32::from(val) & 1) << 23)) & 0xFF8000;
            }
            0x7F00..=0x7FFF => self.psg.write(val),
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

    #[test]
    fn new_core_is_not_paused() {
        let core = GenesisCore::new();
        assert!(!core.paused());
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
}

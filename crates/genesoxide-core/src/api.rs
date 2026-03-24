//! The core emulator API surface.
//!
//! Provides the primary interface for controlling the Genesis emulator.
//! The [`GenesisCore`] struct is the main entry point for host applications.
//! Frontends drive it via [`Command`] and poll state via [`CoreQuery`].

use crate::bus;
use crate::cpu::Cpu;
use crate::io::ControllerPort;
use crate::rom::{self, RomHeader};
use crate::scheduler::Scheduler;
use crate::vdp::Vdp;

/// Genesis visible frame width in pixels (H40 mode).
pub const FRAME_WIDTH: usize = 320;
/// Genesis visible frame height in pixels (NTSC).
pub const FRAME_HEIGHT: usize = 224;
/// Framebuffer byte count for RGBA8 format.
pub const FRAME_RGBA_BYTES: usize = FRAME_WIDTH * FRAME_HEIGHT * 4;
/// NTSC frame rate in millihertz (59.92 Hz * 1000).
pub const FPS_MILLI: u32 = 59_920;

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
        // TODO: fetch, decode, execute one instruction
        // For now, just advance PC and cycles as a stub
        self.cpu.pc = self.cpu.pc.wrapping_add(2);
        self.cpu.cycles += 4;
        self.scheduler.advance_cpu(4);
    }

    fn step_scanline(&mut self) {
        // TODO: run CPU for one scanline's worth of cycles
        // Genesis H40: ~488 68K cycles per scanline
        for _ in 0..488 / 4 {
            self.step_cpu();
        }
    }

    fn step_frame(&mut self) {
        if self.paused {
            return;
        }
        for _ in 0..SCANLINES_PER_FRAME {
            self.step_scanline();
        }
        self.port1.reset_th_counter();
        self.port2.reset_th_counter();
        self.frame_count += 1;
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
            _ => 0, // TODO: implement remaining regions
        }
    }
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
}

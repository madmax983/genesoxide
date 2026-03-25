//! The core emulator API surface.
//!
//! Provides the primary interface for controlling the Genesis emulator.
//! The [`GenesisCore`] struct is the main entry point for host applications.
//! Frontends drive it via [`Command`] and poll state via [`CoreQuery`].

use crate::bus;
use crate::cpu::execute::Bus;
use crate::cpu::{self, Cpu};
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

    /// Returns a VDP snapshot for debugging.
    #[must_use]
    pub fn vdp_snapshot(&self) -> crate::vdp::VdpSnapshot {
        self.vdp.snapshot()
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

        // Clear V-blank at frame start
        self.vdp.set_vblank(false);

        for scanline in 0..SCANLINES_PER_FRAME {
            // Begin scanline timing
            self.vdp.begin_scanline(scanline);

            // Run CPU for this scanline
            self.step_scanline();

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
                    };
                    let cycles = cpu::deliver_interrupt(&mut self.cpu, &mut bus, 6);
                    self.cpu.cycles += u64::from(cycles);
                    self.scheduler.advance_cpu(u64::from(cycles));
                }
            }
        }

        self.vdp.end_frame();
        self.port1.reset_th_counter();
        self.port2.reset_th_counter();
        self.frame_count += 1;
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
                    0x01 => self.port1.read_data(),
                    0x03 => self.port2.read_data(),
                    0x05 => self.port1.read_ctrl(),
                    0x07 => self.port2.read_ctrl(),
                    _ => 0,
                }
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
                    0x01 => self.port1.write_data(val),
                    0x03 => self.port2.write_data(val),
                    0x05 => self.port1.write_ctrl(val),
                    0x07 => self.port2.write_ctrl(val),
                    _ => {}
                }
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
                    0x01 => self.port1.read_data(),
                    0x03 => self.port2.read_data(),
                    0x05 => self.port1.read_ctrl(),
                    0x07 => self.port2.read_ctrl(),
                    _ => 0,
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
                // Word reads: high byte is typically 0, low byte is the register
                let reg = (addr & 0x1F) as u8;
                let val = match reg {
                    0x00 | 0x01 => self.port1.read_data(),
                    0x02 | 0x03 => self.port2.read_data(),
                    0x04 | 0x05 => self.port1.read_ctrl(),
                    0x06 | 0x07 => self.port2.read_ctrl(),
                    _ => 0,
                };
                u16::from(val)
            }
            bus::BusRegion::Vdp => {
                let vdp_addr = addr & 0x1F;
                match vdp_addr {
                    0x00 | 0x02 => self.vdp.read_data(),
                    0x04 | 0x06 => self.vdp.read_status(),
                    0x08 | 0x0A | 0x0C | 0x0E => {
                        // HV counter (stub: return 0 for now)
                        0
                    }
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
                    0x01 => self.port1.write_data(val),
                    0x03 => self.port2.write_data(val),
                    0x05 => self.port1.write_ctrl(val),
                    0x07 => self.port2.write_ctrl(val),
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
                    _ => {}
                }
            }
            bus::BusRegion::IoRegisters => {
                let reg = (addr & 0x1F) as u8;
                let lo = val as u8;
                match reg {
                    0x00 | 0x01 => self.port1.write_data(lo),
                    0x02 | 0x03 => self.port2.write_data(lo),
                    0x04 | 0x05 => self.port1.write_ctrl(lo),
                    0x06 | 0x07 => self.port2.write_ctrl(lo),
                    _ => {}
                }
            }
            _ => {}
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

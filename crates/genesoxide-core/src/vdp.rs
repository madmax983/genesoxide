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

use crate::api::FRAME_RGBA_BYTES;

/// VRAM size in bytes.
pub const VRAM_SIZE: usize = 0x10000; // 64KB
/// Number of CRAM entries (9-bit RGB color values stored as u16).
pub const CRAM_ENTRIES: usize = 64; // 4 palettes x 16 colors
/// Number of VSRAM entries (vertical scroll values).
pub const VSRAM_ENTRIES: usize = 40;
/// Number of VDP registers.
pub const VDP_REGISTER_COUNT: usize = 24;

/// VDP access type set by the control port command words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VdpSnapshot {
    pub vram: Vec<u8>,
    pub cram: Vec<u16>,
    pub vsram: Vec<u16>,
    pub registers: Vec<u8>,
    pub control_state: ControlState,
    pub address: u16,
    pub scanline: u16,
    pub dot: u16,
}

/// Maximum sprites evaluated per frame (H40 mode).
const MAX_SPRITES_TOTAL: usize = 80;
/// Maximum sprites per scanline in H40 mode.
const MAX_SPRITES_PER_LINE: usize = 20;

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
    /// Current scanline (0-261 NTSC).
    scanline: u16,
    /// Current dot within scanline.
    dot: u16,
    /// RGBA framebuffer.
    framebuffer: Box<[u8; FRAME_RGBA_BYTES]>,
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
    /// Combined control code (CD5-CD0) from two-word command sequence.
    control_code: u8,
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
            framebuffer: Box::new([0; FRAME_RGBA_BYTES]),
            in_vblank: false,
            in_hblank: false,
            dma_pending: false,
            dma_fill_pending: false,
            h_interrupt_counter: 0,
            control_code: 0,
        }
    }

    /// Returns a reference to the RGBA framebuffer.
    #[must_use]
    pub fn framebuffer(&self) -> &[u8; FRAME_RGBA_BYTES] {
        &self.framebuffer
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

    /// Writes to the VDP control port.
    ///
    /// Handles the two-word command sequence and register writes.
    pub fn write_control(&mut self, value: u16) {
        match self.control_state {
            ControlState::Idle => {
                // Check if this is a register write: 100x xxxx xxxx xxxx
                if value & 0xC000 == 0x8000 {
                    let reg = ((value >> 8) & 0x1F) as usize;
                    let data = (value & 0xFF) as u8;
                    if reg < VDP_REGISTER_COUNT {
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
    pub fn write_data(&mut self, value: u16) {
        // Reset control state on data port access
        self.control_state = ControlState::Idle;

        // Check if this data write triggers a VRAM fill DMA
        if self.dma_fill_pending {
            self.dma_fill_pending = false;
            self.execute_dma_fill(value);
            return;
        }

        match self.access_type {
            Some(AccessType::VramWrite) => {
                let addr = self.address as usize;
                if addr < VRAM_SIZE - 1 {
                    self.vram[addr] = (value >> 8) as u8;
                    self.vram[addr + 1] = value as u8;
                }
            }
            Some(AccessType::CramWrite) => {
                let index = (self.address >> 1) as usize;
                if index < CRAM_ENTRIES {
                    self.cram[index] = value & 0x0EEE; // 9-bit: 0BBB0GGG0RRR
                }
            }
            Some(AccessType::VsramWrite) => {
                let index = (self.address >> 1) as usize;
                if index < VSRAM_ENTRIES {
                    self.vsram[index] = value & 0x07FF; // 11-bit scroll value
                }
            }
            _ => {}
        }
        self.address = self.address.wrapping_add(self.auto_increment);
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

    /// Reads the VDP status register.
    #[must_use]
    pub fn read_status(&self) -> u16 {
        let mut status: u16 = 0x3400; // Always set bits
        if self.in_vblank {
            status |= 0x0008;
        }
        if self.in_hblank {
            status |= 0x0004;
        }
        status
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

        // H-interrupt counter management
        if line == 0 {
            // Reload at the start of each frame
            self.h_interrupt_counter = i16::from(self.registers[0x0A]);
        }
    }

    /// Called at the end of a visible frame. Resets scanline counter.
    pub fn end_frame(&mut self) {
        self.scanline = 0;
        self.in_hblank = false;
    }

    // ---- Rendering ----

    /// Returns the horizontal screen width based on the current mode.
    /// H40 = 320 pixels, H32 = 256 pixels.
    #[must_use]
    fn screen_width(&self) -> u16 {
        if self.registers[0x0C] & 0x81 != 0 {
            320
        } else {
            256
        }
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
    #[allow(dead_code)]
    fn window_nametable_addr(&self) -> usize {
        usize::from(self.registers[0x03] & 0x3E) << 10
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
    /// 3. Render Scroll A plane
    /// 4. Render sprites
    /// 5. Handle priority: high-priority tiles/sprites draw over low-priority
    pub fn render_scanline(&mut self, line: u16) {
        let width = self.screen_width();
        if line >= 224 || !self.display_enabled() {
            // During V-blank or if display disabled, fill with background
            let bg = self.background_color();
            let y = line as usize;
            if y < 224 {
                for x in 0..320usize {
                    let offset = (y * 320 + x) * 4;
                    self.framebuffer[offset..offset + 4].copy_from_slice(&bg);
                }
            }
            return;
        }

        let (h_cells, v_cells) = self.scroll_size();
        let (hscroll_a, hscroll_b) = self.hscroll_for_line(line);
        let nt_a = self.scroll_a_nametable_addr();
        let nt_b = self.scroll_b_nametable_addr();

        let bg_color = self.background_color();
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

        // Step 1: Background fill
        for pixel in pixel_color.iter_mut().take(width as usize) {
            *pixel = bg_color;
        }

        // Step 2: Scroll B (lowest priority plane)
        for x in 0..width {
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

        // Step 3: Scroll A
        for x in 0..width {
            let vscroll_a = self.vscroll_for_column(x / 8, 0);
            let plane_y = line.wrapping_add(vscroll_a) % (v_cells * 8);
            let plane_x = (x as i16).wrapping_sub(hscroll_a) as u16 % (h_cells * 8);

            if let Some((color, pri)) =
                self.render_plane_pixel(nt_a, plane_x, plane_y, h_cells, v_cells)
            {
                let xi = x as usize;
                let pri_level = if pri { 2 } else { 1 };
                // Scroll A draws over Scroll B at same or higher priority
                if pri_level >= pixel_priority[xi] {
                    pixel_color[xi] = color;
                    pixel_priority[xi] = pri_level;
                }
            }
        }

        // Step 4: Sprites
        self.render_sprites_on_line(line, width, &mut pixel_color, &mut pixel_priority);

        // Step 5: Left column blank (register 0, bit 5)
        if self.registers[0] & 0x20 != 0 {
            for pixel in pixel_color.iter_mut().take(8) {
                *pixel = bg_color;
            }
        }

        // Write final pixel data to framebuffer
        for (x, color) in pixel_color.iter().enumerate() {
            let offset = (y * 320 + x) * 4;
            self.framebuffer[offset..offset + 4].copy_from_slice(color);
        }

        self.in_hblank = false;
    }

    /// Renders sprites that intersect the given scanline.
    fn render_sprites_on_line(
        &self,
        line: u16,
        width: u16,
        pixel_color: &mut [[u8; 4]; 320],
        pixel_priority: &mut [u8; 320],
    ) {
        let sat_base = self.sprite_table_addr();
        let mut sprites_on_line: usize = 0;

        // Walk the sprite link list
        let mut sprite_index: u8 = 0;
        let mut sprites_visited: usize = 0;

        loop {
            if sprites_visited >= MAX_SPRITES_TOTAL {
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
                if sprites_on_line >= MAX_SPRITES_PER_LINE {
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
                        let pri_level = if priority { 2 } else { 1 };

                        // Sprites with higher priority level overwrite.
                        // Among same priority, earlier sprites in link list win (already drawn).
                        if pri_level >= pixel_priority[xi] {
                            pixel_color[xi] = self.resolve_color(palette, color_index);
                            pixel_priority[xi] = pri_level;
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
        }
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

    #[test]
    fn register_write() {
        let mut vdp = Vdp::new();
        // Write 0x42 to register 5: command = 0x8542
        vdp.write_control(0x8542);
        assert_eq!(vdp.registers[5], 0x42);
    }

    #[test]
    fn auto_increment_register() {
        let mut vdp = Vdp::new();
        // Set auto-increment (reg 0x0F) to 2
        vdp.write_control(0x8F02);
        assert_eq!(vdp.auto_increment, 2);
    }

    #[test]
    fn vram_write_and_read() {
        let mut vdp = Vdp::new();
        // Set auto-increment to 2
        vdp.write_control(0x8F02);
        // Set up VRAM write to address 0x0000
        // First word:  CD1-0=01 (VRAM write), A13-0=0x0000 -> 0x4000
        // Second word: CD5-2=0000, A15-14=00 -> 0x0000
        vdp.write_control(0x4000);
        vdp.write_control(0x0000);
        // Write data
        vdp.write_data(0xABCD);

        // Set up VRAM read from address 0x0000
        vdp.write_control(0x0000);
        vdp.write_control(0x0000);
        let val = vdp.read_data();
        assert_eq!(val, 0xABCD);
    }

    #[test]
    fn cram_write_masks_to_9bit() {
        let mut vdp = Vdp::new();
        vdp.write_control(0x8F02);
        // CRAM write: CD=0011 -> first word has CD1-0=11, addr=0
        vdp.write_control(0xC000);
        vdp.write_control(0x0000);
        vdp.write_data(0xFFFF);
        // Should be masked to 0x0EEE
        vdp.write_control(0x0000); // CRAM read setup
        vdp.write_control(0x0020); // CD5-2=1000
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
        let offset = (0 * 320 + 160) * 4;
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
            for col in 0..4 {
                row[col] = 1; // red
            }
            for col in 4..8 {
                row[col] = 2; // blue
            }
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
        vdp.write_data(0xFF00);

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
        vdp.write_control(0x4000);
        vdp.write_control(0x0080);

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
}

//! Minimal 68000 test-ROM builder for headless VDP golden-frame tests.
//!
//! This module emits tiny, self-contained Sega Genesis / Mega Drive ROM images
//! that consist of nothing but a reset vector and a flat stream of VDP register
//! and memory writes, terminated by an infinite self-loop. Running such a ROM
//! through the real emulator (`run_rom_frames`) drives the actual
//! CPU -> bus -> VDP path and produces a genuine rendered framebuffer, which is
//! exactly what the video golden tests compare against.
//!
//! All code here is original and freely licensed, so the ROMs it produces carry
//! no third-party licensing constraints — they can be checked in and regenerated
//! freely.
//!
//! # 68000 / Genesis facts encoded here
//!
//! * On reset the 68000 reads the initial supervisor stack pointer from ROM
//!   bytes `0x000000..0x000004` and the initial PC from `0x000004..0x000008`,
//!   both big-endian. We use `SP = 0x00FF0000`, `PC = 0x00000200`. Code lives at
//!   ROM offset `0x200`.
//! * VDP data port  = `0xC00000` (word), VDP control port = `0xC00004` (word).
//! * Register write (to the control port): word = `0x8000 | (reg << 8) | value8`.
//! * A VDP memory write address is programmed by writing a 32-bit command as two
//!   words (high word first) to the control port. The command layout is
//!   `((CD & 3) << 30) | ((addr & 0x3FFF) << 16) | (((CD >> 2) & 7) << 4)
//!    | ((addr >> 14) & 3)` where `CD` = `0x01` (VRAM), `0x03` (CRAM),
//!   `0x05` (VSRAM).
//! * After the address is set, each word written to the data port
//!   auto-increments the VDP address by the autoinc value (register `0x0F`).
//!
//! # Opcodes emitted (all big-endian)
//!
//! * `movea.l #imm32, a0` = `0x207C imm32`  (a0 = control port)
//! * `movea.l #imm32, a1` = `0x227C imm32`  (a1 = data port)
//! * `move.w  #imm16, (a0)` = `0x30BC imm16` (write to control port)
//! * `move.w  #imm16, (a1)` = `0x32BC imm16` (write to data port)
//! * `bra.s   *`            = `0x60FE`       (infinite self-loop)

/// VDP control port address (word access).
pub const VDP_CONTROL_PORT: u32 = 0x00C0_0004;
/// VDP data port address (word access).
pub const VDP_DATA_PORT: u32 = 0x00C0_0000;

/// Initial supervisor stack pointer written to the reset vector.
pub const INITIAL_SP: u32 = 0x00FF_0000;
/// Initial program counter written to the reset vector. Code starts here.
pub const CODE_START: u32 = 0x0000_0200;

/// VDP access code (CD) for a VRAM write.
const CD_VRAM_WRITE: u32 = 0x01;
/// VDP access code (CD) for a CRAM write.
const CD_CRAM_WRITE: u32 = 0x03;
/// VDP access code (CD) for a VSRAM write.
const CD_VSRAM_WRITE: u32 = 0x05;

/// Builds the two control-port command words that program a VDP memory write
/// address for the given access code. Returns `(high_word, low_word)`.
#[must_use]
fn address_command(cd: u32, addr: u32) -> (u16, u16) {
    let cmd = ((cd & 0x03) << 30)
        | ((addr & 0x3FFF) << 16)
        | (((cd >> 2) & 0x07) << 4)
        | ((addr >> 14) & 0x03);
    ((cmd >> 16) as u16, (cmd & 0xFFFF) as u16)
}

/// A fluent builder that records a flat stream of VDP writes and assembles them
/// into a bootable 68000 ROM image.
///
/// The recorded stream is emitted verbatim as 68000 instructions after a small
/// prologue that loads `a0` with the control port and `a1` with the data port.
#[derive(Clone)]
pub struct RomBuilder {
    /// Recorded instruction words (already encoded, big-endian order preserved
    /// as native `u16`s; serialized big-endian in `finish`).
    code: Vec<u16>,
    /// Optional interrupt handler code blocks. Each entry is `(vector_addr,
    /// handler_words)`: the handler is appended after the main code and the
    /// 68000 auto-vector at `vector_addr` (e.g. `0x70` for level-4 HINT,
    /// `0x78` for level-6 VINT) is patched to point at it. Empty by default, so
    /// ROMs that use no interrupts are byte-for-byte unchanged.
    handlers: Vec<(u32, Vec<u16>)>,
}

/// 68000 auto-vector address for a level-4 (H-blank) interrupt: `0x60 + 4*4`.
pub const HINT_VECTOR: u32 = 0x0000_0070;
/// 68000 auto-vector address for a level-6 (V-blank) interrupt: `0x60 + 6*4`.
pub const VINT_VECTOR: u32 = 0x0000_0078;

impl Default for RomBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl RomBuilder {
    /// Creates an empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            code: Vec::new(),
            handlers: Vec::new(),
        }
    }

    /// Emits raw, already-encoded big-endian instruction words into the main
    /// code stream (executed after the port-loading prologue). Use this for
    /// instructions the higher-level helpers don't cover, e.g. lowering the
    /// interrupt mask with `move.w #0x2000, sr` (`[0x46FC, 0x2000]`).
    pub fn emit(&mut self, words: &[u16]) -> &mut Self {
        self.code.extend_from_slice(words);
        self
    }

    /// Installs an interrupt handler: `handler_words` is appended after the
    /// main code and the auto-vector at `vector_addr` is patched to point at
    /// it. The handler must end with `rte` (`0x4E73`).
    pub fn set_handler(&mut self, vector_addr: u32, handler_words: &[u16]) -> &mut Self {
        self.handlers.push((vector_addr, handler_words.to_vec()));
        self
    }

    /// `move.w #value, (a0)` — write a raw word to the VDP control port.
    fn write_control(&mut self, value: u16) -> &mut Self {
        self.code.push(0x30BC);
        self.code.push(value);
        self
    }

    /// `move.w #value, (a1)` — write a raw word to the VDP data port.
    fn write_data_port(&mut self, value: u16) -> &mut Self {
        self.code.push(0x32BC);
        self.code.push(value);
        self
    }

    /// Writes an 8-bit value to VDP register `reg`.
    pub fn set_register(&mut self, reg: u8, value: u8) -> &mut Self {
        let word = 0x8000u16 | (u16::from(reg) << 8) | u16::from(value);
        self.write_control(word)
    }

    /// Programs the VDP write address for a subsequent VRAM data stream.
    pub fn set_vram_addr(&mut self, addr: u16) -> &mut Self {
        let (hi, lo) = address_command(CD_VRAM_WRITE, u32::from(addr));
        self.write_control(hi).write_control(lo)
    }

    /// Programs the VDP write address for a subsequent CRAM data stream.
    /// `addr` is the CRAM byte address (color index * 2).
    pub fn set_cram_addr(&mut self, addr: u16) -> &mut Self {
        let (hi, lo) = address_command(CD_CRAM_WRITE, u32::from(addr));
        self.write_control(hi).write_control(lo)
    }

    /// Programs the VDP write address for a subsequent VSRAM data stream.
    pub fn set_vsram_addr(&mut self, addr: u16) -> &mut Self {
        let (hi, lo) = address_command(CD_VSRAM_WRITE, u32::from(addr));
        self.write_control(hi).write_control(lo)
    }

    /// Writes a single data word to the VDP data port (VRAM/CRAM/VSRAM,
    /// depending on the most recent `set_*_addr`).
    pub fn write_data(&mut self, word: u16) -> &mut Self {
        self.write_data_port(word)
    }

    /// Writes a slice of data words to the VDP data port.
    pub fn write_data_slice(&mut self, words: &[u16]) -> &mut Self {
        for &w in words {
            self.write_data_port(w);
        }
        self
    }

    /// Convenience: program a VRAM address then stream `words` into it.
    pub fn write_vram(&mut self, addr: u16, words: &[u16]) -> &mut Self {
        self.set_vram_addr(addr).write_data_slice(words)
    }

    /// Convenience: program a CRAM byte address then stream `words` into it.
    pub fn write_cram(&mut self, addr: u16, words: &[u16]) -> &mut Self {
        self.set_cram_addr(addr).write_data_slice(words)
    }

    /// Convenience: write a single CRAM color entry by color index (0..63).
    pub fn set_cram_color(&mut self, index: u16, color: u16) -> &mut Self {
        self.write_cram(index * 2, &[color])
    }

    /// Convenience: write a full 8x8 4bpp tile pattern at `tile_index`.
    ///
    /// `rows` holds 8 rows of 8 4-bit color indices. Each row is packed into
    /// four VRAM bytes / two words, high nibble = leftmost pixel.
    pub fn write_tile(&mut self, tile_index: u16, rows: &[[u8; 8]; 8]) -> &mut Self {
        let base = tile_index * 32;
        self.set_vram_addr(base);
        for row in rows {
            // Two words per row (4 bytes = 8 pixels).
            let b0 = (u16::from(row[0] & 0xF) << 12)
                | (u16::from(row[1] & 0xF) << 8)
                | (u16::from(row[2] & 0xF) << 4)
                | u16::from(row[3] & 0xF);
            let b1 = (u16::from(row[4] & 0xF) << 12)
                | (u16::from(row[5] & 0xF) << 8)
                | (u16::from(row[6] & 0xF) << 4)
                | u16::from(row[7] & 0xF);
            self.write_data_port(b0);
            self.write_data_port(b1);
        }
        self
    }

    /// Convenience: fill a tile with a single solid color index.
    pub fn write_solid_tile(&mut self, tile_index: u16, color_index: u8) -> &mut Self {
        self.write_tile(tile_index, &[[color_index; 8]; 8])
    }

    /// Assembles the recorded stream into a padded, bootable ROM image.
    ///
    /// Layout:
    /// * `0x000..0x004` initial SSP (big-endian)
    /// * `0x004..0x008` initial PC  (big-endian) = `CODE_START`
    /// * `0x200..`      prologue (`movea.l` into a0/a1) + recorded stream
    /// * trailing       `bra.s *` self-loop, then zero-padding to a power of two
    #[must_use]
    pub fn finish(&self) -> Vec<u8> {
        let mut words: Vec<u16> = Vec::new();

        // Prologue at CODE_START: load the two VDP port addresses.
        // movea.l #VDP_CONTROL_PORT, a0
        words.push(0x207C);
        words.push((VDP_CONTROL_PORT >> 16) as u16);
        words.push((VDP_CONTROL_PORT & 0xFFFF) as u16);
        // movea.l #VDP_DATA_PORT, a1
        words.push(0x227C);
        words.push((VDP_DATA_PORT >> 16) as u16);
        words.push((VDP_DATA_PORT & 0xFFFF) as u16);

        // Recorded instruction stream.
        words.extend_from_slice(&self.code);

        // Infinite self-loop.
        words.push(0x60FE);

        // Append any interrupt handlers after the self-loop and remember each
        // one's absolute 68000 address (ROM maps at 0x000000, so the handler's
        // address equals its ROM byte offset). The vectors are patched below.
        let code_start = CODE_START as usize;
        let mut handler_addrs: Vec<(u32, u32)> = Vec::new();
        for (vector_addr, handler) in &self.handlers {
            let handler_addr = (code_start + words.len() * 2) as u32;
            handler_addrs.push((*vector_addr, handler_addr));
            words.extend_from_slice(handler);
        }

        // Serialize code bytes (big-endian) starting at CODE_START.
        let mut rom = vec![0u8; code_start + words.len() * 2];
        for (i, w) in words.iter().enumerate() {
            let off = code_start + i * 2;
            rom[off] = (w >> 8) as u8;
            rom[off + 1] = (*w & 0xFF) as u8;
        }

        // Reset vectors.
        write_be32(&mut rom, 0x0000, INITIAL_SP);
        write_be32(&mut rom, 0x0004, CODE_START);

        // Interrupt auto-vectors (patched only when handlers were installed).
        for (vector_addr, handler_addr) in handler_addrs {
            write_be32(&mut rom, vector_addr as usize, handler_addr);
        }

        // Pad up to at least 0x400, rounded to the next power of two.
        let min_len = 0x400usize.max(rom.len());
        let padded = min_len.next_power_of_two();
        rom.resize(padded, 0);
        rom
    }
}

/// Writes a big-endian 32-bit value into `buf` at `off`.
fn write_be32(buf: &mut [u8], off: usize, value: u32) {
    buf[off] = (value >> 24) as u8;
    buf[off + 1] = (value >> 16) as u8;
    buf[off + 2] = (value >> 8) as u8;
    buf[off + 3] = (value & 0xFF) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_vectors_are_big_endian() {
        let rom = RomBuilder::new().finish();
        // SSP at 0x0
        assert_eq!(&rom[0..4], &[0x00, 0xFF, 0x00, 0x00]);
        // PC at 0x4 = 0x00000200
        assert_eq!(&rom[4..8], &[0x00, 0x00, 0x02, 0x00]);
    }

    #[test]
    fn prologue_loads_ports() {
        let rom = RomBuilder::new().finish();
        let base = CODE_START as usize;
        // movea.l #imm32, a0
        assert_eq!(rom[base], 0x20);
        assert_eq!(rom[base + 1], 0x7C);
        assert_eq!(&rom[base + 2..base + 6], &[0x00, 0xC0, 0x00, 0x04]);
        // movea.l #imm32, a1
        assert_eq!(rom[base + 6], 0x22);
        assert_eq!(rom[base + 7], 0x7C);
        assert_eq!(&rom[base + 8..base + 12], &[0x00, 0xC0, 0x00, 0x00]);
    }

    #[test]
    fn register_write_encoding() {
        let mut b = RomBuilder::new();
        b.set_register(0x01, 0x44);
        // move.w #0x8144, (a0)
        assert_eq!(b.code, vec![0x30BC, 0x8144]);
    }

    #[test]
    fn vram_address_command_math() {
        // VRAM write @ 0x0000 -> high 0x4000, low 0x0000
        assert_eq!(address_command(CD_VRAM_WRITE, 0x0000), (0x4000, 0x0000));
        // VRAM write @ 0xC000 -> A=0xC000: (A&0x3FFF)=0x0000, (A>>14)&3 = 3
        assert_eq!(address_command(CD_VRAM_WRITE, 0xC000), (0x4000, 0x0003));
        // CRAM write @ 0x0000 -> high 0xC000
        assert_eq!(address_command(CD_CRAM_WRITE, 0x0000), (0xC000, 0x0000));
    }

    #[test]
    fn rom_is_power_of_two_padded() {
        let rom = RomBuilder::new().finish();
        assert!(rom.len().is_power_of_two());
        assert!(rom.len() >= 0x400);
    }

    #[test]
    fn tile_packs_high_nibble_left() {
        let mut b = RomBuilder::new();
        let mut rows = [[0u8; 8]; 8];
        rows[0] = [1, 2, 3, 4, 5, 6, 7, 8];
        b.write_tile(1, &rows);
        // After the two set_vram_addr control words, first data word packs
        // 1,2,3,4 -> 0x1234.
        // code: [ctrl_hi, ctrl_lo] then [0x32BC, 0x1234] ...
        assert_eq!(b.code[0], 0x30BC); // set_vram_addr high control write opcode
        // find first data write opcode 0x32BC
        let idx = b.code.iter().position(|&w| w == 0x32BC).unwrap();
        assert_eq!(b.code[idx + 1], 0x1234);
    }
}

//! Genesis controller I/O.
//!
//! The Genesis uses a TH (select line) toggling protocol to read button
//! state from controllers. The 68000 writes to control/data registers at
//! 0xA10001-0xA1000F to configure and read the ports.
//!
//! 3-button pad: Up/Down/Left/Right/A/B/C/Start
//! 6-button pad: adds X/Y/Z/Mode via a multi-step TH toggle sequence

use serde::{Deserialize, Serialize};

/// Controller port (1 or 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Port {
    Player1,
    Player2,
}

/// Genesis controller buttons.
///
/// Bit positions match the hardware protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Button {
    Up,
    Down,
    Left,
    Right,
    A,
    B,
    C,
    Start,
    // 6-button extensions
    X,
    Y,
    Z,
    Mode,
}

/// Physical pad type plugged into a controller port.
///
/// A `ThreeButton` pad only ever exposes the classic two-state (TH high/low)
/// read; a `SixButton` pad additionally emits the X/Y/Z/Mode extras through the
/// multi-step TH-toggle sequence decoded in [`ControllerPort::read_data`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PadType {
    /// Classic 3-button pad (Up/Down/Left/Right/A/B/C/Start).
    ThreeButton,
    /// 6-button pad (adds X/Y/Z/Mode).
    #[default]
    SixButton,
}

/// Idle timeout, in 68000 cycles, after which the 6-button pad's TH toggle
/// counter resets. The real pad resets ~1.5 ms after the last TH edge;
/// 7.67 MHz × 0.0015 s ≈ 11 500 cycles.
pub const SIX_BUTTON_TIMEOUT_CYCLES: u32 = 11_500;

/// State of a single controller port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerPort {
    /// Raw button state — bit set = pressed.
    buttons: u16,
    /// TH line state (output from 68000).
    th_state: bool,
    /// TH toggle count for 6-button detection.
    th_count: u8,
    /// Control register value.
    ctrl: u8,
    /// Which physical pad is plugged in.
    #[serde(default)]
    pad_type: PadType,
    /// 68000 cycles elapsed since the last TH edge. Used to reset `th_count`
    /// after the 6-button idle timeout so the phase machine is deterministic.
    #[serde(default)]
    cycles_since_th_write: u32,
}

impl ControllerPort {
    /// Creates a new controller port with no buttons pressed.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buttons: 0,
            th_state: true,
            th_count: 0,
            ctrl: 0,
            pad_type: PadType::default(),
            cycles_since_th_write: 0,
        }
    }

    /// Sets the physical pad type plugged into this port.
    pub fn set_pad_type(&mut self, pad_type: PadType) {
        self.pad_type = pad_type;
    }

    /// Returns the physical pad type plugged into this port.
    #[must_use]
    pub fn pad_type(&self) -> PadType {
        self.pad_type
    }

    /// Presses a button.
    pub fn press(&mut self, button: Button) {
        self.buttons |= button_mask(button);
    }

    /// Releases a button.
    pub fn release(&mut self, button: Button) {
        self.buttons &= !button_mask(button);
    }

    /// Sets the full button state from a bitmask.
    pub fn set_buttons(&mut self, mask: u16) {
        self.buttons = mask;
    }

    /// Returns the current raw button state bitmask.
    #[must_use]
    pub fn buttons(&self) -> u16 {
        self.buttons
    }

    /// TH=1 3-button read byte: Up/Down/Left/Right on bits 0-3, B/C on bits
    /// 4-5, TH readback (0x40) set. Active low (not pressed = bit set).
    fn three_button_high(b: u16) -> u8 {
        let mut val = 0u8;
        if !pressed(b, Button::Up) {
            val |= 0x01;
        }
        if !pressed(b, Button::Down) {
            val |= 0x02;
        }
        if !pressed(b, Button::Left) {
            val |= 0x04;
        }
        if !pressed(b, Button::Right) {
            val |= 0x08;
        }
        if !pressed(b, Button::B) {
            val |= 0x10;
        }
        if !pressed(b, Button::C) {
            val |= 0x20;
        }
        val | 0x40 // TH readback = 1
    }

    /// TH=0 3-button read byte: Up/Down on bits 0-1, A/Start on bits 4-5.
    /// Bits 2-3 read 0 (Left/Right grounded), TH readback (0x40) clear.
    fn three_button_low(b: u16) -> u8 {
        let mut val = 0u8;
        if !pressed(b, Button::Up) {
            val |= 0x01;
        }
        if !pressed(b, Button::Down) {
            val |= 0x02;
        }
        if !pressed(b, Button::A) {
            val |= 0x10;
        }
        if !pressed(b, Button::Start) {
            val |= 0x20;
        }
        // Bits 2-3 are 0 when TH=0 (active low = Left/Right grounded)
        val
    }

    /// Reads the data port value based on current TH state (and, for a
    /// 6-button pad, the TH toggle count).
    ///
    /// Returns the byte that the 68000 reads from the port. The TH line
    /// selects which group of buttons is visible.
    ///
    /// # 6-button phase table (`phase` = `th_count`)
    ///
    /// A 6-button pad is polled by cycling TH high→low repeatedly. `th_count`
    /// counts TH rising edges, so the reads a game performs map to phases as
    /// follows (bit 6 = TH readback; low nibble = bits 3..0):
    ///
    /// | phase | TH | low nibble d3 d2 d1 d0 | meaning              |
    /// |-------|----|------------------------|----------------------|
    /// | 0     | 1  | Right Left Down Up     | 3-button high (1st)  |
    /// | 0     | 0  | 0 0 Down Up            | 3-button low  (1st)  |
    /// | 1     | 1  | Right Left Down Up     | 3-button high (2nd)  |
    /// | 1     | 0  | 0 0 Down Up            | 3-button low  (2nd)  |
    /// | 2     | 1  | Right Left Down Up     | 3-button high (3rd)  |
    /// | 2     | 0  | 0 0 0 0                | 6-button present sig |
    /// | 3     | 1  | 1 1 1 1                | acknowledge signature|
    /// | 3     | 0  | Mode X Y Z             | extra buttons        |
    /// | ≥4    | -  | (3-button)             | revert until reset   |
    ///
    /// A 3-button pad always uses the phase-independent two-state behavior.
    #[must_use]
    pub fn read_data(&self) -> u8 {
        let b = self.buttons;

        // 3-button pad: never consult th_count.
        if self.pad_type == PadType::ThreeButton {
            return if self.th_state {
                Self::three_button_high(b)
            } else {
                Self::three_button_low(b)
            };
        }

        match self.th_count {
            // Phase 2, TH=0: directions (low nibble) forced to 0. Games read
            // this "all directions grounded" pattern to detect a 6-button pad.
            // A/Start (bits 4-5) still report normally.
            2 if !self.th_state => Self::three_button_low(b) & 0x30,
            // Phase 3, TH=1: low nibble = 1111 signature; B/C (bits 4-5) and
            // the TH readback (bit 6) report normally.
            3 if self.th_state => (Self::three_button_high(b) & 0x70) | 0x0F,
            // Phase 3, TH=0: extra buttons on the low nibble (active low):
            // bit3=Mode, bit2=X, bit1=Y, bit0=Z. A/Start (bits 4-5) normal.
            3 => {
                let mut val = Self::three_button_low(b) & 0x30;
                if !pressed(b, Button::Mode) {
                    val |= 0x08;
                }
                if !pressed(b, Button::X) {
                    val |= 0x04;
                }
                if !pressed(b, Button::Y) {
                    val |= 0x02;
                }
                if !pressed(b, Button::Z) {
                    val |= 0x01;
                }
                val
            }
            // Phases 0, 1 and 4+ : plain 3-button behavior.
            _ => {
                if self.th_state {
                    Self::three_button_high(b)
                } else {
                    Self::three_button_low(b)
                }
            }
        }
    }

    /// Writes to the data port (sets TH line and output bits).
    pub fn write_data(&mut self, value: u8) {
        let new_th = value & 0x40 != 0;
        if new_th != self.th_state {
            // Any TH edge is activity — restart the idle timeout.
            self.cycles_since_th_write = 0;
        }
        if !self.th_state && new_th {
            // TH rising edge — increment toggle count for 6-button detection
            self.th_count = self.th_count.wrapping_add(1);
        }
        self.th_state = new_th;
    }

    /// Advances the idle timeout by `cycles` 68000 cycles. When TH has been
    /// static for longer than [`SIX_BUTTON_TIMEOUT_CYCLES`], the 6-button
    /// phase counter resets so a stalled poll cannot leave the pad latched in a
    /// half-completed extended-read sequence. Called from the CPU step loop.
    pub fn advance_cycles(&mut self, cycles: u32) {
        self.cycles_since_th_write = self.cycles_since_th_write.saturating_add(cycles);
        if self.cycles_since_th_write >= SIX_BUTTON_TIMEOUT_CYCLES {
            self.th_count = 0;
        }
    }

    /// Writes to the control register.
    pub fn write_ctrl(&mut self, value: u8) {
        self.ctrl = value;
    }

    /// Reads the control register.
    #[must_use]
    pub fn read_ctrl(&self) -> u8 {
        self.ctrl
    }

    /// Resets the TH toggle counter (called once per frame).
    pub fn reset_th_counter(&mut self) {
        self.th_count = 0;
    }
}

impl Default for ControllerPort {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns the bitmask for a button.
#[must_use]
const fn button_mask(button: Button) -> u16 {
    match button {
        Button::Up => 1 << 0,
        Button::Down => 1 << 1,
        Button::Left => 1 << 2,
        Button::Right => 1 << 3,
        Button::A => 1 << 4,
        Button::B => 1 << 5,
        Button::C => 1 << 6,
        Button::Start => 1 << 7,
        Button::X => 1 << 8,
        Button::Y => 1 << 9,
        Button::Z => 1 << 10,
        Button::Mode => 1 << 11,
    }
}

/// Checks if a button is pressed in the bitmask.
#[must_use]
const fn pressed(buttons: u16, button: Button) -> bool {
    buttons & button_mask(button) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_buttons_pressed_reads_all_high() {
        let port = ControllerPort::new();
        // TH=1: all direction/button bits should be 1 (not pressed = active low)
        let data = port.read_data();
        assert_eq!(data & 0x3F, 0x3F);
    }

    #[test]
    fn pressing_b_clears_bit() {
        let mut port = ControllerPort::new();
        port.press(Button::B);
        let data = port.read_data(); // TH=1
        // B is bit 4 when TH=1 — should be 0 when pressed
        assert_eq!(data & 0x10, 0);
    }

    #[test]
    fn th_toggle_reads_different_buttons() {
        let mut port = ControllerPort::new();
        port.press(Button::A);
        port.press(Button::C);

        // TH=1: should see C (bit 5) pressed, A not visible
        let data_th1 = port.read_data();
        assert_eq!(data_th1 & 0x20, 0); // C pressed

        // TH=0: should see A (bit 4) pressed
        port.write_data(0x00); // TH=0
        let data_th0 = port.read_data();
        assert_eq!(data_th0 & 0x10, 0); // A pressed
    }

    /// Drives the exact TH toggle sequence a game uses to poll a 6-button pad
    /// and asserts the precise byte returned at each phase.
    #[test]
    fn six_button_phase_sequence_exact_bytes() {
        let mut port = ControllerPort::new();
        assert_eq!(port.pad_type(), PadType::SixButton); // default

        // Press a distinct mix: A, Right, X, Mode.
        port.press(Button::A);
        port.press(Button::Right);
        port.press(Button::X);
        port.press(Button::Mode);

        // three_button_high = 0x77 (Right pressed clears bit3),
        // three_button_low  = 0x23 (A pressed clears bit4).

        // Phase 0, TH=1 (initial state).
        assert_eq!(port.read_data(), 0x77);

        port.write_data(0x00); // TH=0, phase 0
        assert_eq!(port.read_data(), 0x23);

        port.write_data(0x40); // TH=1 rising -> phase 1
        assert_eq!(port.read_data(), 0x77);
        port.write_data(0x00); // TH=0, phase 1
        assert_eq!(port.read_data(), 0x23);

        port.write_data(0x40); // TH=1 rising -> phase 2
        assert_eq!(port.read_data(), 0x77);
        port.write_data(0x00); // TH=0, phase 2: 6-button-present signature
        // Low nibble forced to 0; A/Start (bits4-5) normal => 0x20.
        assert_eq!(port.read_data(), 0x20);

        port.write_data(0x40); // TH=1 rising -> phase 3: 1111 acknowledge
        // (0x77 & 0x70) | 0x0F = 0x7F.
        assert_eq!(port.read_data(), 0x7F);
        port.write_data(0x00); // TH=0, phase 3: extra buttons
        // A/Start bits => 0x20; Mode(bit3)/X(bit2) pressed clear,
        // Y(bit1)/Z(bit0) not pressed set => 0x20 | 0x02 | 0x01 = 0x23.
        assert_eq!(port.read_data(), 0x23);

        // Phase 4+ reverts to plain 3-button reads.
        port.write_data(0x40); // TH=1 rising -> phase 4
        assert_eq!(port.read_data(), 0x77);
        port.write_data(0x00); // TH=0, phase 4
        assert_eq!(port.read_data(), 0x23);
    }

    /// The 3rd TH=0 read exposes all-zero directions and the 4th TH=0 read the
    /// X/Y/Z/Mode extras — verified independently of the directional buttons.
    #[test]
    fn six_button_signature_and_extras_with_no_directions() {
        let mut port = ControllerPort::new();
        port.press(Button::X);
        port.press(Button::Y);
        port.press(Button::Z);
        port.press(Button::Mode);

        // Advance to phase 2 TH=0 (signature).
        port.write_data(0x00);
        port.write_data(0x40); // phase 1
        port.write_data(0x00);
        port.write_data(0x40); // phase 2
        port.write_data(0x00);
        // No directions and nothing on the low nibble => low nibble 0.
        assert_eq!(port.read_data() & 0x0F, 0x00);

        // Phase 3 TH=1 acknowledge = 1111.
        port.write_data(0x40);
        assert_eq!(port.read_data() & 0x0F, 0x0F);

        // Phase 3 TH=0: all four extras pressed => low nibble all 0.
        port.write_data(0x00);
        assert_eq!(port.read_data() & 0x0F, 0x00);
    }

    /// After the idle timeout elapses the 6-button phase counter resets, so the
    /// next read reverts to first-phase 3-button behavior.
    #[test]
    fn idle_timeout_resets_phase_counter() {
        let mut port = ControllerPort::new();
        port.press(Button::Mode); // would show in extras phase

        // Drive to phase 3 TH=0 (extras visible).
        port.write_data(0x00);
        port.write_data(0x40); // phase 1
        port.write_data(0x00);
        port.write_data(0x40); // phase 2
        port.write_data(0x00);
        port.write_data(0x40); // phase 3
        port.write_data(0x00);
        // Mode pressed => bit3 clear in the extras nibble.
        assert_eq!(port.read_data() & 0x08, 0x00);

        // Simulate elapsed cycles beyond the timeout.
        port.advance_cycles(SIX_BUTTON_TIMEOUT_CYCLES);

        // Before the reset the extras nibble was 0x37 (X/Y/Z high, Mode low);
        // after the reset TH=0 reads plain 3-button low = 0x33, since Mode (a
        // 6-button extra) never appears in a 3-button read.
        assert_eq!(port.read_data(), 0x33);

        // And the next full poll behaves like the first phase again.
        port.write_data(0x40); // TH=1, phase 1 after reset
        assert_eq!(port.read_data(), 0x7F);
    }

    /// A 3-button pad never emits the 6-button phases regardless of how many
    /// TH toggles occur.
    #[test]
    fn three_button_pad_never_emits_extended_phases() {
        let mut port = ControllerPort::new();
        port.set_pad_type(PadType::ThreeButton);
        port.press(Button::X); // must never appear
        port.press(Button::Mode);

        // Drive many full TH cycles.
        for _ in 0..8 {
            port.write_data(0x40); // TH=1
            let hi = port.read_data();
            // Always the plain 3-button-high byte (0x40 readback set, low
            // nibble = directions all high since none pressed).
            assert_eq!(hi, 0x7F);
            port.write_data(0x00); // TH=0
            let lo = port.read_data();
            // Plain 3-button-low: no A/Start/dir pressed => 0x33; never the
            // 1111 or extras signatures.
            assert_eq!(lo, 0x33);
        }
    }

    /// Pad type (and the extended-read behavior it enables) survives a
    /// serde snapshot round-trip.
    #[test]
    fn pad_type_survives_snapshot() {
        let mut port = ControllerPort::new();
        port.set_pad_type(PadType::ThreeButton);
        let json = serde_json::to_string(&port).unwrap();
        let restored: ControllerPort = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.pad_type(), PadType::ThreeButton);
        assert_eq!(restored, port);

        let mut six = ControllerPort::new();
        six.set_pad_type(PadType::SixButton);
        let json = serde_json::to_string(&six).unwrap();
        let restored: ControllerPort = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.pad_type(), PadType::SixButton);
    }
}

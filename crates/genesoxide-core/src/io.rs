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
        }
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

    /// Reads the data port value based on current TH state.
    ///
    /// Returns the byte that the 68000 reads from the port.
    /// The TH line selects which group of buttons is visible.
    #[must_use]
    pub fn read_data(&self) -> u8 {
        let b = self.buttons;
        if self.th_state {
            // TH=1: Up/Down/Left/Right/B/C (active low)
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
        } else {
            // TH=0: Up/Down/A/Start (active low)
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
    }

    /// Writes to the data port (sets TH line and output bits).
    pub fn write_data(&mut self, value: u8) {
        let new_th = value & 0x40 != 0;
        if !self.th_state && new_th {
            // TH rising edge — increment toggle count for 6-button detection
            self.th_count = self.th_count.wrapping_add(1);
        }
        self.th_state = new_th;
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
}

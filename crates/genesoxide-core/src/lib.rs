//! Genesoxide core emulation library.
//!
//! Pure Genesis/Mega Drive emulation with no I/O dependencies.
//! Frontends drive the emulator via [`Command`] and poll state via [`CoreQuery`].

pub mod api;
pub mod bus;
pub mod cpu;
pub mod io;
pub mod rom;
pub mod scheduler;
pub mod vdp;

pub use api::{Button, Command, CoreQuery, GenesisCore};
pub use api::{FRAME_HEIGHT, FRAME_RGBA_BYTES, FRAME_WIDTH};

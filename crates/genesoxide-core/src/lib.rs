//! Genesoxide core emulation library.
//!
//! Pure Genesis/Mega Drive emulation with no I/O dependencies.
//! Frontends drive the emulator via [`Command`] and poll state via [`CoreQuery`].

pub mod api;
pub mod bus;
pub mod cpu;
pub mod eeprom;
pub mod io;
pub mod psg;
pub mod rewind;
pub mod rom;
pub mod scheduler;
pub mod timing;
pub mod vdp;
pub mod ym2612;
pub mod z80;

pub use api::{
    AudioEqKind, AudioEqStage, AudioOutputConfig, AudioOutputProfile, Button, Command, CoreQuery,
    GenesisCore, GenesisCoreSnapshot, PadType,
};
pub use api::{
    FRAME_HEIGHT, FRAME_PERIOD_NS, FRAME_PERIOD_NS_PAL, FRAME_RGBA_BYTES, FRAME_RGBA_BYTES_MAX,
    FRAME_WIDTH, MAX_FRAME_HEIGHT,
};
pub use scheduler::Region;
pub use rewind::{
    ArrayDelta, CompressedTimeline, FrameDelta, FrameInput, KeyframePolicy, RewindBuffer,
    RewindConfig, RewindStatus,
};

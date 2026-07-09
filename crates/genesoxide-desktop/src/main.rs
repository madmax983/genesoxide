//! Genesoxide desktop frontend.
//!
//! CLI-first Genesis emulator. No GUI chrome, just run the ROM.
//!
//! ```text
//! genesoxide run sonic.bin --scale 3
//! genesoxide info sonic.bin
//! ```

mod audio;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use genesoxide_config::GenesisConfig;
use genesoxide_core::{Command, FRAME_HEIGHT, FRAME_PERIOD_NS, FRAME_WIDTH, GenesisCore};
use gilrs::{Button as PadButton, EventType, Gilrs};
use pixels::{Pixels, SurfaceTexture};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

#[derive(Parser)]
#[command(name = "genesoxide", about = "Sega Genesis emulator")]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Run a Genesis ROM.
    Run {
        /// Path to ROM file (or config name).
        rom: String,
        /// Window scale factor.
        #[arg(long, default_value = "3")]
        scale: u32,
        /// Config file path.
        #[arg(long, default_value = "genesoxide.toml")]
        config: PathBuf,
    },
    /// Display ROM header information.
    Info {
        /// Path to ROM file.
        rom: PathBuf,
    },
    /// Verify ROM checksum.
    Verify {
        /// Path to ROM file.
        rom: PathBuf,
    },
    /// Run headless and dump audio output to a WAV file.
    DumpAudio {
        /// Path to ROM file.
        rom: PathBuf,
        /// Output WAV file path.
        #[arg(short, long, default_value = "output.wav")]
        output: PathBuf,
        /// Number of frames to run.
        #[arg(short, long, default_value = "600")]
        frames: u32,
        /// Sample rate in Hz.
        #[arg(long, default_value = "44100")]
        sample_rate: u32,
        /// Skip N frames before recording (lets the game boot first).
        #[arg(long, default_value = "0")]
        skip: u32,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        CliCommand::Run { rom, scale, config } => cmd_run(&rom, scale, &config),
        CliCommand::Info { rom } => cmd_info(&rom),
        CliCommand::Verify { rom } => cmd_verify(&rom),
        CliCommand::DumpAudio {
            rom,
            output,
            frames,
            sample_rate,
            skip,
        } => cmd_dump_audio(&rom, &output, frames, sample_rate, skip),
    }
}

/// How often (in frames) to flush dirty SRAM to disk during play, so a crash
/// or forced kill does not lose recent progress. ~3 seconds at 60 Hz.
const SRAM_FLUSH_INTERVAL: u32 = 180;

/// Derives the `.srm` battery-save path for a ROM. When `saves_dir` is set the
/// file lives there (named after the ROM stem); otherwise it sits next to the
/// ROM with its extension replaced by `.srm`.
fn srm_path_for(rom_path: &Path, saves_dir: Option<&str>) -> PathBuf {
    match saves_dir {
        Some(dir) => {
            let stem = rom_path
                .file_stem()
                .map(std::ffi::OsString::from)
                .unwrap_or_default();
            let mut file = stem;
            file.push(".srm");
            Path::new(dir).join(file)
        }
        None => rom_path.with_extension("srm"),
    }
}

/// Writes the core's SRAM to `srm_path` if it holds data worth persisting.
fn flush_sram(core: &GenesisCore, srm_path: &Path) {
    if !core.sram_worth_saving() {
        return;
    }
    if let Some(parent) = srm_path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    if let Err(e) = fs::write(srm_path, core.sram()) {
        eprintln!("Warning: failed to write SRAM {}: {e}", srm_path.display());
    }
}

fn cmd_run(rom_name: &str, scale: u32, config_path: &Path) -> Result<()> {
    let config = GenesisConfig::load(config_path).unwrap_or_default();
    let rom_path = config.resolve_rom(rom_name);
    let rom_data = fs::read(&rom_path)
        .with_context(|| format!("Failed to read ROM: {}", rom_path.display()))?;

    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom_data));

    // Battery-save persistence: derive the `.srm` path and load any existing
    // save into the cartridge SRAM before the game boots.
    let srm_path = srm_path_for(&rom_path, config.desktop.saves_dir.as_deref());
    if srm_path.exists() {
        match fs::read(&srm_path) {
            Ok(bytes) => {
                core.load_sram(&bytes);
                eprintln!("Loaded SRAM: {}", srm_path.display());
            }
            Err(e) => eprintln!(
                "Warning: failed to read SRAM {}: {e}",
                srm_path.display()
            ),
        }
    }

    // Apply the configured rewind settings (mapping the config-crate struct to
    // the core struct).
    core.execute(Command::SetRewindConfig(genesoxide_core::RewindConfig {
        enabled: config.rewind.enabled,
        keyframe_base_interval: config.rewind.keyframe_base_interval,
        max_history_seconds: config.rewind.max_history_seconds,
        delta_spike_threshold: config.rewind.delta_spike_threshold,
    }));

    // Select each port's pad type from config so the core decodes 6-button
    // extras (X/Y/Z/Mode) only for ports configured for a 6-button pad.
    core.execute(Command::SetPadType {
        port: 0,
        six_button: config.desktop.pad1_six_button,
    });
    core.execute(Command::SetPadType {
        port: 1,
        six_button: config.desktop.pad2_six_button,
    });

    if let Some(header) = core.rom_header() {
        eprintln!("Loaded: {}", header.title_overseas);
        eprintln!("Region: {}", header.region);
    }

    // Gamepad input via gilrs (optional — keyboard still works without one).
    let gilrs = match Gilrs::new() {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("Warning: gamepad support unavailable: {e}");
            None
        }
    };

    let event_loop = EventLoop::new().context("Failed to create event loop")?;
    event_loop.set_control_flow(ControlFlow::Poll);

    let audio = audio::AudioOutput::open();
    if let Some(ref a) = audio {
        core.execute(Command::SetAudioSampleRate(a.sample_rate()));
    } else {
        eprintln!("Warning: no audio output device available");
    }

    let mut app = App {
        core,
        scale,
        window: None,
        pixels: None,
        audio,
        last_frame_time: None,
        frame_duration: Duration::from_nanos(FRAME_PERIOD_NS),
        rewind_held: false,
        paused: false,
        srm_path,
        frames_since_flush: 0,
        gilrs,
        last_dims: (FRAME_WIDTH as u32, FRAME_HEIGHT as u32),
    };

    event_loop.run_app(&mut app).context("Event loop error")?;

    // Final save-on-exit flush (covers a clean event-loop return).
    flush_sram(&app.core, &app.srm_path);
    app.core.clear_sram_dirty();
    Ok(())
}

struct App {
    core: GenesisCore,
    scale: u32,
    window: Option<Window>,
    pixels: Option<Pixels<'static>>,
    audio: Option<audio::AudioOutput>,
    last_frame_time: Option<Instant>,
    frame_duration: Duration,
    /// True while Backspace is held (hold-to-rewind).
    rewind_held: bool,
    /// True when emulation is paused (P toggles).
    paused: bool,
    /// Battery-save file path for this ROM.
    srm_path: PathBuf,
    /// Frames elapsed since the last periodic SRAM flush.
    frames_since_flush: u32,
    /// Gamepad input context (`None` when unavailable).
    gilrs: Option<Gilrs>,
    /// Last framebuffer dimensions the Pixels buffer was sized to. Used to
    /// detect an H32<->H40 mode switch at a frame boundary and resize the
    /// Pixels texture buffer to match the core's native-width framebuffer.
    last_dims: (u32, u32),
}

/// Maps a gilrs gamepad button to a Genesis controller button.
///
/// Face buttons South/East/West/North and the two shoulders map to the six
/// Genesis face buttons A/B/C/X/Y/Z; Start→Start, Select→Mode, and the D-pad to
/// directions. Returns `None` for buttons with no Genesis equivalent.
fn pad_button_to_genesis(button: PadButton) -> Option<genesoxide_core::Button> {
    use genesoxide_core::Button as G;
    Some(match button {
        PadButton::South => G::A,
        PadButton::East => G::B,
        PadButton::West => G::C,
        PadButton::North => G::X,
        PadButton::LeftTrigger | PadButton::LeftTrigger2 => G::Y,
        PadButton::RightTrigger | PadButton::RightTrigger2 => G::Z,
        PadButton::Start => G::Start,
        PadButton::Select | PadButton::Mode => G::Mode,
        PadButton::DPadUp => G::Up,
        PadButton::DPadDown => G::Down,
        PadButton::DPadLeft => G::Left,
        PadButton::DPadRight => G::Right,
        _ => return None,
    })
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let size = LogicalSize::new(
            FRAME_WIDTH as u32 * self.scale,
            FRAME_HEIGHT as u32 * self.scale,
        );

        let attrs = Window::default_attributes()
            .with_title("genesoxide")
            .with_inner_size(size)
            .with_min_inner_size(LogicalSize::new(FRAME_WIDTH as u32, FRAME_HEIGHT as u32));

        let window = event_loop
            .create_window(attrs)
            .expect("Failed to create window");

        // Store window first, then create pixels from the stored reference
        self.window = Some(window);
        let window_ref = self.window.as_ref().unwrap();
        let physical = window_ref.inner_size();
        let surface = SurfaceTexture::new(physical.width, physical.height, window_ref);
        let pixels = Pixels::new(FRAME_WIDTH as u32, FRAME_HEIGHT as u32, surface)
            .expect("Failed to create pixel buffer");

        // SAFETY: pixels lifetime is tied to self.window which we keep alive
        // for the duration of the App. Window is never moved or dropped while
        // pixels exists.
        self.pixels =
            Some(unsafe { std::mem::transmute::<pixels::Pixels<'_>, pixels::Pixels<'_>>(pixels) });
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                flush_sram(&self.core, &self.srm_path);
                self.core.clear_sram_dirty();
                event_loop.exit();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(key) = event.physical_key {
                    let pressed = event.state.is_pressed();
                    let button = match key {
                        KeyCode::ArrowUp => Some(genesoxide_core::Button::Up),
                        KeyCode::ArrowDown => Some(genesoxide_core::Button::Down),
                        KeyCode::ArrowLeft => Some(genesoxide_core::Button::Left),
                        KeyCode::ArrowRight => Some(genesoxide_core::Button::Right),
                        KeyCode::KeyZ => Some(genesoxide_core::Button::A),
                        KeyCode::KeyX => Some(genesoxide_core::Button::B),
                        KeyCode::KeyC => Some(genesoxide_core::Button::C),
                        // 6-button extras: A/S/D row above Z/X/C, Shift = Mode.
                        KeyCode::KeyA => Some(genesoxide_core::Button::X),
                        KeyCode::KeyS => Some(genesoxide_core::Button::Y),
                        KeyCode::KeyD => Some(genesoxide_core::Button::Z),
                        KeyCode::ShiftLeft => Some(genesoxide_core::Button::Mode),
                        KeyCode::Enter => Some(genesoxide_core::Button::Start),
                        KeyCode::Escape => {
                            flush_sram(&self.core, &self.srm_path);
                            self.core.clear_sram_dirty();
                            event_loop.exit();
                            None
                        }
                        // Backspace: hold-to-rewind.
                        KeyCode::Backspace => {
                            self.rewind_held = pressed;
                            None
                        }
                        // P: toggle pause (only on key press).
                        KeyCode::KeyP => {
                            if pressed {
                                self.paused = !self.paused;
                                self.core.execute(if self.paused {
                                    Command::Pause
                                } else {
                                    Command::Resume
                                });
                            }
                            None
                        }
                        // '.' or F: single-frame step forward while paused.
                        KeyCode::Period | KeyCode::KeyF => {
                            if pressed && self.paused {
                                // step_frame() is a no-op while the core is
                                // paused, so momentarily resume for one frame.
                                self.core.execute(Command::Resume);
                                self.core.execute(Command::StepFrame);
                                self.core.execute(Command::Pause);
                            }
                            None
                        }
                        // ',' : single-frame step backward.
                        KeyCode::Comma => {
                            if pressed {
                                self.core.execute(Command::StepBack);
                            }
                            None
                        }
                        _ => None,
                    };

                    if let Some(btn) = button {
                        let cmd = if pressed {
                            Command::PressButton {
                                port: 0,
                                button: btn,
                            }
                        } else {
                            Command::ReleaseButton {
                                port: 0,
                                button: btn,
                            }
                        };
                        self.core.execute(cmd);
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                if self.rewind_held {
                    self.core.execute(Command::StepBack);
                } else if !self.paused {
                    self.core.execute(Command::StepFrame);
                }

                // Periodically flush dirty battery SRAM so a crash does not
                // discard recent saves.
                self.frames_since_flush = self.frames_since_flush.saturating_add(1);
                if self.frames_since_flush >= SRAM_FLUSH_INTERVAL && self.core.sram_is_dirty() {
                    flush_sram(&self.core, &self.srm_path);
                    self.core.clear_sram_dirty();
                    self.frames_since_flush = 0;
                }

                // Push audio samples to output
                if let Some(audio) = &mut self.audio {
                    let samples = self.core.audio_samples();
                    audio.push_samples(samples);
                    self.core.clear_audio_buffer();
                }

                // Copy framebuffer to pixel surface. The core framebuffer is
                // native-width (256px in H32, 320px in H40), so on a mode switch
                // — applied at this frame boundary — resize the Pixels texture
                // buffer to match before copying. The window/surface physical
                // size is left unchanged; Pixels scales the narrower H32 buffer
                // up to the surface for us.
                if let Some(pixels) = &mut self.pixels {
                    let dims = self.core.framebuffer_dimensions();
                    if dims != self.last_dims {
                        if pixels.resize_buffer(dims.0, dims.1).is_ok() {
                            self.last_dims = dims;
                        }
                    }
                    let fb = self.core.framebuffer_rgba();
                    pixels.frame_mut().copy_from_slice(fb);
                    let _ = pixels.render();
                }

                self.last_frame_time = Some(Instant::now());
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Drain gamepad events and feed them to controller port 0.
        if let Some(gilrs) = &mut self.gilrs {
            while let Some(event) = gilrs.next_event() {
                let (button, pressed) = match event.event {
                    EventType::ButtonPressed(b, _) => (b, true),
                    EventType::ButtonReleased(b, _) => (b, false),
                    _ => continue,
                };
                if let Some(btn) = pad_button_to_genesis(button) {
                    let cmd = if pressed {
                        Command::PressButton { port: 0, button: btn }
                    } else {
                        Command::ReleaseButton { port: 0, button: btn }
                    };
                    self.core.execute(cmd);
                }
            }
        }

        if let Some(last) = self.last_frame_time {
            let elapsed = last.elapsed();
            if elapsed < self.frame_duration {
                std::thread::sleep(self.frame_duration - elapsed);
            }
        }

        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

fn cmd_info(rom_path: &PathBuf) -> Result<()> {
    let data =
        fs::read(rom_path).with_context(|| format!("Failed to read: {}", rom_path.display()))?;
    let header = genesoxide_core::rom::parse_header(&data).map_err(|e| anyhow::anyhow!("{e}"))?;

    println!("System:    {}", header.system_type);
    println!("Title:     {}", header.title_overseas);
    println!("Domestic:  {}", header.title_domestic);
    println!("Serial:    {}", header.serial);
    println!("Copyright: {}", header.copyright);
    println!("Region:    {}", header.region);
    println!(
        "ROM:       0x{:06X}-0x{:06X} ({:.1} KB)",
        header.rom_start,
        header.rom_end,
        (header.rom_end - header.rom_start + 1) as f64 / 1024.0
    );
    println!("Checksum:  0x{:04X} (header)", header.checksum);

    let computed = genesoxide_core::rom::compute_checksum(&data);
    let valid = header.checksum == computed;
    println!(
        "Computed:  0x{:04X} {}",
        computed,
        if valid { "(OK)" } else { "(MISMATCH)" }
    );

    Ok(())
}

fn cmd_verify(rom_path: &PathBuf) -> Result<()> {
    let data =
        fs::read(rom_path).with_context(|| format!("Failed to read: {}", rom_path.display()))?;

    if genesoxide_core::rom::verify_checksum(&data) {
        println!("OK: checksum valid");
        Ok(())
    } else {
        bail!("FAIL: checksum mismatch")
    }
}

fn cmd_dump_audio(
    rom_path: &Path,
    output_path: &Path,
    frames: u32,
    sample_rate: u32,
    skip: u32,
) -> Result<()> {
    let rom_data = fs::read(rom_path)
        .with_context(|| format!("Failed to read ROM: {}", rom_path.display()))?;

    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom_data));
    core.execute(Command::SetAudioSampleRate(sample_rate));

    if let Some(header) = core.rom_header() {
        eprintln!("Loaded: {}", header.title_overseas);
    }

    // Skip frames (game boot, etc.) — discard audio
    if skip > 0 {
        eprintln!("Skipping {skip} frames...");
        for _ in 0..skip {
            core.execute(Command::StepFrame);
        }
        core.clear_audio_buffer();
    }

    // Collect audio for the requested number of frames
    eprintln!("Recording {frames} frames at {sample_rate} Hz...");
    let mut all_samples: Vec<f32> = Vec::new();
    for frame in 0..frames {
        core.execute(Command::StepFrame);
        all_samples.extend_from_slice(core.audio_samples());
        core.clear_audio_buffer();

        if (frame + 1) % 100 == 0 {
            eprintln!("  frame {}/{frames}", frame + 1);
        }
    }

    let duration_secs = all_samples.len() as f64 / (2.0 * f64::from(sample_rate));
    eprintln!(
        "Captured {} samples ({:.1}s stereo)",
        all_samples.len(),
        duration_secs
    );

    // Write WAV
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(output_path, spec)
        .with_context(|| format!("Failed to create WAV: {}", output_path.display()))?;

    for &sample in &all_samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let i16_val = (clamped * 32767.0) as i16;
        writer.write_sample(i16_val)?;
    }
    writer.finalize()?;

    eprintln!("Wrote {}", output_path.display());
    Ok(())
}

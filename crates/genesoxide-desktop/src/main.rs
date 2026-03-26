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
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use genesoxide_config::GenesisConfig;
use genesoxide_core::{Command, FRAME_HEIGHT, FRAME_PERIOD_NS, FRAME_WIDTH, GenesisCore};
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        CliCommand::Run { rom, scale, config } => cmd_run(&rom, scale, &config),
        CliCommand::Info { rom } => cmd_info(&rom),
        CliCommand::Verify { rom } => cmd_verify(&rom),
    }
}

fn cmd_run(rom_name: &str, scale: u32, config_path: &PathBuf) -> Result<()> {
    let config = GenesisConfig::load(config_path).unwrap_or_default();
    let rom_path = config.resolve_rom(rom_name);
    let rom_data = fs::read(&rom_path)
        .with_context(|| format!("Failed to read ROM: {}", rom_path.display()))?;

    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom_data));

    if let Some(header) = core.rom_header() {
        eprintln!("Loaded: {}", header.title_overseas);
        eprintln!("Region: {}", header.region);
    }

    let event_loop = EventLoop::new().context("Failed to create event loop")?;
    event_loop.set_control_flow(ControlFlow::Poll);

    let audio = audio::AudioOutput::open();
    if audio.is_none() {
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
    };

    event_loop.run_app(&mut app).context("Event loop error")?;
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
        self.pixels = Some(unsafe { std::mem::transmute(pixels) });
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(key) = event.physical_key {
                    let button = match key {
                        KeyCode::ArrowUp => Some(genesoxide_core::Button::Up),
                        KeyCode::ArrowDown => Some(genesoxide_core::Button::Down),
                        KeyCode::ArrowLeft => Some(genesoxide_core::Button::Left),
                        KeyCode::ArrowRight => Some(genesoxide_core::Button::Right),
                        KeyCode::KeyZ => Some(genesoxide_core::Button::A),
                        KeyCode::KeyX => Some(genesoxide_core::Button::B),
                        KeyCode::KeyC => Some(genesoxide_core::Button::C),
                        KeyCode::Enter => Some(genesoxide_core::Button::Start),
                        KeyCode::Escape => {
                            event_loop.exit();
                            None
                        }
                        _ => None,
                    };

                    if let Some(btn) = button {
                        let cmd = if event.state.is_pressed() {
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
                // Step one frame
                self.core.execute(Command::StepFrame);

                // Push audio samples to output
                if let Some(audio) = &mut self.audio {
                    let samples = self.core.audio_samples();
                    audio.push_samples(samples);
                    self.core.clear_audio_buffer();
                }

                // Copy framebuffer to pixel surface
                if let Some(pixels) = &mut self.pixels {
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

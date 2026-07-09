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

fn cmd_run(rom_name: &str, scale: u32, config_path: &Path) -> Result<()> {
    let config = GenesisConfig::load(config_path).unwrap_or_default();
    let rom_path = config.resolve_rom(rom_name);
    let rom_data = fs::read(&rom_path)
        .with_context(|| format!("Failed to read ROM: {}", rom_path.display()))?;

    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom_data));

    // Apply the configured rewind settings (mapping the config-crate struct to
    // the core struct).
    core.execute(Command::SetRewindConfig(genesoxide_core::RewindConfig {
        enabled: config.rewind.enabled,
        keyframe_base_interval: config.rewind.keyframe_base_interval,
        max_history_seconds: config.rewind.max_history_seconds,
        delta_spike_threshold: config.rewind.delta_spike_threshold,
    }));

    if let Some(header) = core.rom_header() {
        eprintln!("Loaded: {}", header.title_overseas);
        eprintln!("Region: {}", header.region);
    }

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
        audio_log_counter: 0,
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
    /// True while Backspace is held (hold-to-rewind).
    rewind_held: bool,
    /// True when emulation is paused (P toggles).
    paused: bool,
    /// Frame counter for periodic audio diagnostics logging.
    audio_log_counter: u32,
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
            WindowEvent::CloseRequested => event_loop.exit(),
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
                        KeyCode::Enter => Some(genesoxide_core::Button::Start),
                        KeyCode::Escape => {
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

                // Push audio samples to output (rate-matched to the device clock).
                if let Some(audio) = &mut self.audio {
                    let samples = self.core.audio_samples();
                    audio.push_frame(samples);
                    self.core.clear_audio_buffer();

                    // Periodically report audio-path health (~ every 2 seconds).
                    self.audio_log_counter += 1;
                    if self.audio_log_counter >= 120 {
                        self.audio_log_counter = 0;
                        let d = audio.diagnostics();
                        eprintln!(
                            "Audio: fill {}/{} ({:.0}% of target {}) underruns={} dropped={} ratio={:.4}",
                            d.fill,
                            d.capacity,
                            if d.target_fill > 0 {
                                d.fill as f32 / d.target_fill as f32 * 100.0
                            } else {
                                0.0
                            },
                            d.target_fill,
                            d.underruns,
                            d.dropped,
                            d.last_ratio,
                        );
                    }
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

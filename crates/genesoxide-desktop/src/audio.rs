//! Audio output via cpal with a lock-free ring buffer.
//!
//! Opens the default audio output device at 44100 Hz stereo and feeds
//! it from a [`ringbuf`] SPSC ring buffer. The emulator pushes samples
//! each frame; the cpal callback pops them. If the ring is empty the
//! callback outputs silence; if the ring is full, overflow samples are
//! silently dropped to prevent latency buildup.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::{
    HeapRb,
    traits::{Consumer, Producer, Split},
};

/// Manages the cpal audio stream and its producer-side ring buffer handle.
pub struct AudioOutput {
    /// Keep the stream alive; dropping it stops playback.
    _stream: cpal::Stream,
    /// Producer half of the ring buffer fed to the cpal callback.
    producer: ringbuf::HeapProd<f32>,
    /// Device sample rate in Hz.
    sample_rate: u32,
}

impl AudioOutput {
    /// Opens the default audio device and starts a 44100 Hz stereo stream.
    ///
    /// Returns `None` if no output device is available or the stream fails
    /// to start (e.g. no audio hardware).
    pub fn open() -> Option<Self> {
        let host = cpal::default_host();
        eprintln!("Audio: using host {:?}", host.id());

        let device = match host.default_output_device() {
            Some(d) => {
                eprintln!("Audio: default device: {:?}", d.name().unwrap_or_default());
                d
            }
            None => {
                // Try enumerating all output devices as fallback
                eprintln!("Audio: no default output device, enumerating...");
                match host.output_devices() {
                    Ok(mut devices) => match devices.next() {
                        Some(d) => {
                            eprintln!(
                                "Audio: using fallback device: {:?}",
                                d.name().unwrap_or_default()
                            );
                            d
                        }
                        None => {
                            eprintln!("Audio: no output devices found at all");
                            return None;
                        }
                    },
                    Err(e) => {
                        eprintln!("Audio: failed to enumerate devices: {e}");
                        return None;
                    }
                }
            }
        };

        // Query device's preferred config and use its sample rate
        let default_config = match device.default_output_config() {
            Ok(c) => {
                eprintln!(
                    "Audio: device default config: {} ch, {} Hz, {:?}",
                    c.channels(),
                    c.sample_rate().0,
                    c.sample_format()
                );
                c
            }
            Err(e) => {
                eprintln!("Audio: failed to get default config: {e}");
                return None;
            }
        };

        let config = cpal::StreamConfig {
            channels: 2,
            sample_rate: default_config.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };
        eprintln!("Audio: opening stream at {} Hz", config.sample_rate.0);

        // Ring buffer: ~4 frames worth of stereo samples.
        let ring = HeapRb::<f32>::new(8192);
        let (producer, mut consumer) = ring.split();

        let stream = match device.build_output_stream(
            &config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for sample in data.iter_mut() {
                    *sample = consumer.try_pop().unwrap_or(0.0);
                }
            },
            |err| eprintln!("Audio stream error: {err}"),
            None,
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Audio: failed to build stream: {e}");
                return None;
            }
        };

        if let Err(e) = stream.play() {
            eprintln!("Audio: failed to start playback: {e}");
            return None;
        }

        Some(Self {
            _stream: stream,
            producer,
            sample_rate: config.sample_rate.0,
        })
    }

    /// Returns the actual sample rate of the audio device.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Pushes stereo interleaved f32 samples into the ring buffer.
    ///
    /// Overflow samples are silently dropped to prevent latency buildup.
    pub fn push_samples(&mut self, samples: &[f32]) {
        for &s in samples {
            let _ = self.producer.try_push(s);
        }
    }
}

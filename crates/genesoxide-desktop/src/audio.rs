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
}

impl AudioOutput {
    /// Opens the default audio device and starts a 44100 Hz stereo stream.
    ///
    /// Returns `None` if no output device is available or the stream fails
    /// to start (e.g. no audio hardware).
    pub fn open() -> Option<Self> {
        let host = cpal::default_host();
        let device = host.default_output_device()?;

        let config = cpal::StreamConfig {
            channels: 2,
            sample_rate: cpal::SampleRate(44100),
            buffer_size: cpal::BufferSize::Default,
        };

        // Ring buffer: ~4 frames worth of stereo samples.
        let ring = HeapRb::<f32>::new(8192);
        let (producer, mut consumer) = ring.split();

        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    for sample in data.iter_mut() {
                        *sample = consumer.try_pop().unwrap_or(0.0);
                    }
                },
                |err| eprintln!("Audio error: {err}"),
                None,
            )
            .ok()?;

        stream.play().ok()?;

        Some(Self {
            _stream: stream,
            producer,
        })
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

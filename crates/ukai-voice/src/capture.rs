//! Recording the default input device.

use std::sync::{Arc, Mutex, PoisonError};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};

use crate::audio::{self, MAX_SECONDS, WHISPER_RATE};

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("no microphone found")]
    NoDevice,
    #[error("microphone: {0}")]
    Device(String),
    #[error("microphone sample format `{0}` is not supported")]
    Format(SampleFormat),
}

/// The microphone while it records. Recording stops when this is finished
/// or dropped.
pub struct Recorder {
    // Held for its side effect: the stream records until it is dropped.
    _stream: Stream,
    samples: Arc<Mutex<Vec<f32>>>,
    rate: u32,
}

/// What a [`Recorder`] heard: mono samples at `rate` Hz.
#[derive(Debug, Clone, PartialEq)]
pub struct Recording {
    pub samples: Vec<f32>,
    pub rate: u32,
}

impl Recorder {
    /// Start recording the default input device.
    pub fn start() -> Result<Self, CaptureError> {
        let device = cpal::default_host()
            .default_input_device()
            .ok_or(CaptureError::NoDevice)?;
        let supported = device
            .default_input_config()
            .map_err(|e| CaptureError::Device(e.to_string()))?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let samples = Arc::new(Mutex::new(Vec::new()));
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, &config, &samples),
            SampleFormat::I16 => build::<i16>(&device, &config, &samples),
            SampleFormat::I32 => build::<i32>(&device, &config, &samples),
            SampleFormat::U16 => build::<u16>(&device, &config, &samples),
            SampleFormat::I8 => build::<i8>(&device, &config, &samples),
            SampleFormat::U8 => build::<u8>(&device, &config, &samples),
            other => return Err(CaptureError::Format(other)),
        }?;
        stream
            .play()
            .map_err(|e| CaptureError::Device(e.to_string()))?;
        Ok(Self {
            _stream: stream,
            samples,
            rate: config.sample_rate,
        })
    }

    /// Stop recording and hand over what was heard.
    pub fn finish(self) -> Recording {
        let samples =
            std::mem::take(&mut *self.samples.lock().unwrap_or_else(PoisonError::into_inner));
        Recording {
            samples,
            rate: self.rate,
        }
    }
}

impl Recording {
    /// The recording as whisper takes it: 16 kHz mono.
    pub fn for_whisper(&self) -> Vec<f32> {
        audio::resample(&self.samples, self.rate, WHISPER_RATE)
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    samples: &Arc<Mutex<Vec<f32>>>,
) -> Result<Stream, CaptureError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels);
    let limit = config.sample_rate as usize * MAX_SECONDS as usize;
    let samples = Arc::clone(samples);
    device
        .build_input_stream(
            *config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                let frame: Vec<f32> = data.iter().map(|s| f32::from_sample(*s)).collect();
                let mono = audio::downmix(&frame, channels);
                let mut samples = samples.lock().unwrap_or_else(PoisonError::into_inner);
                let room = limit.saturating_sub(samples.len());
                samples.extend(mono.into_iter().take(room));
            },
            |error| tracing::warn!("microphone: {error}"),
            None,
        )
        .map_err(|e| CaptureError::Device(e.to_string()))
}

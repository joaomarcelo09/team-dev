use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Indexing, Resampler as RubatoResampler, SincInterpolationParameters,
    SincInterpolationType, WindowFunction,
};

use crate::error::VoiceError;

pub const TARGET_SAMPLE_RATE: u32 = 16_000;
const DEFAULT_CHUNK_SIZE: usize = 1024;

/// A general streaming resampler converting audio from `from_rate` to `to_rate` mono `f32`.
pub struct Resampler {
    from_rate: u32,
    to_rate: u32,
    resampler: Option<Async<f32>>,
    input_buffer: Vec<f32>,
    out_scratch: Vec<f32>,
}

impl Resampler {
    /// Creates a new streaming resampler from `from_rate` to `to_rate` mono.
    ///
    /// If `from_rate == to_rate`, the resampler operates in passthrough mode.
    pub fn new(from_rate: u32, to_rate: u32) -> Result<Self, VoiceError> {
        if from_rate == 0 || to_rate == 0 {
            return Err(VoiceError::Other("sample rates must be > 0".into()));
        }

        if from_rate == to_rate {
            return Ok(Self {
                from_rate,
                to_rate,
                resampler: None,
                input_buffer: Vec::new(),
                out_scratch: Vec::new(),
            });
        }

        let params = SincInterpolationParameters::new(64, WindowFunction::Blackman2)
            .oversampling_factor(128)
            .interpolation(SincInterpolationType::Linear);

        let ratio = to_rate as f64 / from_rate as f64;
        let resampler = Async::<f32>::new_sinc(
            ratio,
            1.1,
            &params,
            DEFAULT_CHUNK_SIZE,
            1,
            FixedAsync::Input,
        )
        .map_err(|e| VoiceError::Other(format!("failed to initialize resampler: {e:?}")))?;

        let max_out = resampler.output_frames_max();
        Ok(Self {
            from_rate,
            to_rate,
            resampler: Some(resampler),
            input_buffer: Vec::with_capacity(DEFAULT_CHUNK_SIZE * 2),
            out_scratch: vec![0.0; max_out],
        })
    }

    /// Returns the input sample rate.
    pub fn from_rate(&self) -> u32 {
        self.from_rate
    }

    /// Returns the target output sample rate.
    pub fn to_rate(&self) -> u32 {
        self.to_rate
    }

    /// Feeds incoming mono `f32` samples and returns any completed resampled frames.
    pub fn process(&mut self, input: &[f32]) -> Result<Vec<f32>, VoiceError> {
        let Some(resampler) = self.resampler.as_mut() else {
            return Ok(input.to_vec());
        };

        self.input_buffer.extend_from_slice(input);
        let chunk_size = resampler.input_frames_next();
        let mut output = Vec::new();

        while self.input_buffer.len() >= chunk_size {
            let chunk: Vec<f32> = self.input_buffer.drain(..chunk_size).collect();
            let in_adapter = InterleavedSlice::new(&chunk, 1, chunk_size)
                .map_err(|e| VoiceError::Other(format!("audio adapter error: {e:?}")))?;
            let out_capacity = self.out_scratch.len();
            let mut out_adapter = InterleavedSlice::new_mut(
                &mut self.out_scratch,
                1,
                out_capacity,
            )
            .map_err(|e| VoiceError::Other(format!("audio adapter error: {e:?}")))?;

            let (_read, written) = resampler
                .process_into_buffer(&in_adapter, &mut out_adapter, None)
                .map_err(|e| VoiceError::Other(format!("resampling error: {e:?}")))?;

            output.extend_from_slice(&self.out_scratch[..written]);
        }

        Ok(output)
    }

    /// Flushes any buffered samples remaining in the resampler.
    pub fn flush(&mut self) -> Result<Vec<f32>, VoiceError> {
        let Some(resampler) = self.resampler.as_mut() else {
            return Ok(Vec::new());
        };

        if self.input_buffer.is_empty() {
            return Ok(Vec::new());
        }

        let chunk_size = resampler.input_frames_next();
        let partial_len = self.input_buffer.len();
        self.input_buffer.resize(chunk_size, 0.0);

        let in_adapter = InterleavedSlice::new(&self.input_buffer, 1, chunk_size)
            .map_err(|e| VoiceError::Other(format!("audio adapter error: {e:?}")))?;
        let out_capacity = self.out_scratch.len();
        let mut out_adapter = InterleavedSlice::new_mut(
            &mut self.out_scratch,
            1,
            out_capacity,
        )
        .map_err(|e| VoiceError::Other(format!("audio adapter error: {e:?}")))?;

        let indexing = Indexing::new().partial_len(partial_len);
        let (_read, written) = resampler
            .process_into_buffer(&in_adapter, &mut out_adapter, Some(&indexing))
            .map_err(|e| VoiceError::Other(format!("resampling error: {e:?}")))?;

        let ratio = self.to_rate as f64 / self.from_rate as f64;
        let actual_frames = ((partial_len as f64 * ratio).round() as usize).min(written);

        self.input_buffer.clear();
        Ok(self.out_scratch[..actual_frames].to_vec())
    }
}

/// Helper function to resample an entire audio buffer from `from_rate` to `to_rate`.
pub fn resample(from_rate: u32, to_rate: u32, input: &[f32]) -> Result<Vec<f32>, VoiceError> {
    if from_rate == to_rate || input.is_empty() {
        return Ok(input.to_vec());
    }
    let mut resampler = Resampler::new(from_rate, to_rate)?;
    let mut output = resampler.process(input)?;
    let flushed = resampler.flush()?;
    output.extend(flushed);
    Ok(output)
}

/// A streaming resampler converting any input sample rate to 16 kHz mono `f32`.
pub struct Resampler16k {
    inner: Resampler,
}

impl Resampler16k {
    /// Creates a new streaming resampler from `from_rate` to 16 kHz mono.
    pub fn new(from_rate: u32) -> Result<Self, VoiceError> {
        Ok(Self {
            inner: Resampler::new(from_rate, TARGET_SAMPLE_RATE)?,
        })
    }

    /// Returns the input sample rate.
    pub fn from_rate(&self) -> u32 {
        self.inner.from_rate()
    }

    /// Feeds incoming mono `f32` samples and returns any completed 16 kHz mono frames.
    pub fn process(&mut self, input: &[f32]) -> Result<Vec<f32>, VoiceError> {
        self.inner.process(input)
    }

    /// Flushes any buffered samples remaining in the resampler.
    pub fn flush(&mut self) -> Result<Vec<f32>, VoiceError> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    #[test]
    fn test_passthrough_16k() {
        let mut resampler = Resampler16k::new(16_000).unwrap();
        let input = vec![0.1, 0.2, 0.3, 0.4];
        let output = resampler.process(&input).unwrap();
        assert_eq!(output, input);
        let flushed = resampler.flush().unwrap();
        assert!(flushed.is_empty());
    }

    #[test]
    fn test_resample_48k_to_16k_sine_1s() {
        // 1 second of 440 Hz sine wave at 48 kHz = 48,000 samples.
        let rate_in = 48_000;
        let mut sine_48k = Vec::with_capacity(rate_in);
        for i in 0..rate_in {
            let t = i as f32 / rate_in as f32;
            sine_48k.push((2.0 * PI * 440.0 * t).sin());
        }

        let mut resampler = Resampler16k::new(rate_in as u32).unwrap();

        // Feed in chunks of 512 samples to test streaming behavior.
        let mut output_16k = Vec::new();
        for chunk in sine_48k.chunks(512) {
            let res = resampler.process(chunk).unwrap();
            output_16k.extend(res);
        }
        let flushed = resampler.flush().unwrap();
        output_16k.extend(flushed);

        // Expected output is 16,000 samples. Within tolerance (+/- 50 samples).
        let len = output_16k.len();
        assert!(
            (len as i32 - 16_000).abs() <= 50,
            "Expected ~16000 samples, got {len}"
        );
    }

    #[test]
    fn test_upsample_22050_to_48k_sine_1s() {
        // 1 second of 440 Hz sine wave at 22,050 Hz = 22,050 samples.
        let rate_in = 22_050;
        let rate_out = 48_000;
        let mut sine_22k = Vec::with_capacity(rate_in);
        for i in 0..rate_in {
            let t = i as f32 / rate_in as f32;
            sine_22k.push((2.0 * PI * 440.0 * t).sin());
        }

        let resampled = resample(rate_in as u32, rate_out as u32, &sine_22k).unwrap();
        let len = resampled.len();
        assert!(
            (len as i32 - rate_out).abs() <= 50,
            "Expected ~48000 samples, got {len}"
        );
    }
}

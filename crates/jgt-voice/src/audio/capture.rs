use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::mpsc;

use crate::error::VoiceError;

/// Downmixes multi-channel interleaved audio into single-channel mono audio by averaging channels.
///
/// If `channels` is 0, returns an empty vector.
/// If `channels` is 1, returns a clone of `samples`.
/// If `channels` > 1, chunks the samples into frames of `channels` length and takes their average.
pub fn downmix(samples: &[f32], channels: u16) -> Vec<f32> {
    if channels == 0 {
        return Vec::new();
    }
    if channels == 1 {
        return samples.to_vec();
    }
    let ch = channels as usize;
    let frames = samples.len() / ch;
    let mut out = Vec::with_capacity(frames);
    let inv_ch = 1.0 / (channels as f32);
    for chunk in samples.chunks_exact(ch) {
        let sum: f32 = chunk.iter().sum();
        out.push(sum * inv_ch);
    }
    out
}

/// An active audio capture stream holding the CPAL stream alive.
pub struct AudioCapture {
    _stream: cpal::Stream,
    device_name: String,
    sample_rate: u32,
    channels: u16,
}

impl AudioCapture {
    /// Returns the capture device name.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Returns the native capture sample rate (e.g. 44100 or 48000).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Returns the native input channel count.
    pub fn channels(&self) -> u16 {
        self.channels
    }
}

/// Opens the default input device (or a specific device by name) at its native configuration,
/// downmixes audio frames to mono `f32`, and sends them to `tx`.
///
/// Never blocks in the audio callback. If the channel is full, incoming frames are dropped.
pub fn start_capture(
    device_name: Option<&str>,
    tx: mpsc::Sender<Vec<f32>>,
) -> Result<AudioCapture, VoiceError> {
    let host = cpal::default_host();
    let device = match device_name {
        Some(name) => {
            let devices: Vec<cpal::Device> = host
                .input_devices()
                .map_err(|e| VoiceError::Other(format!("failed to query input devices: {e}")))?
                .collect();
            super::select_device(devices, name)?
        }
        None => host
            .default_input_device()
            .ok_or_else(|| VoiceError::Other("no default input audio device found".into()))?,
    };

    let device_name_str = super::get_device_name(&device);

    let default_config = device
        .default_input_config()
        .map_err(|e| VoiceError::Other(format!("failed to get default input config: {e}")))?;

    let sample_rate = default_config.sample_rate();
    let channels = default_config.channels();
    let sample_format = default_config.sample_format();
    let config: cpal::StreamConfig = default_config.config();

    tracing::info!(
        device = %device_name_str,
        sample_rate,
        channels,
        "input audio capture opened"
    );

    let err_fn = |err| {
        tracing::warn!("audio capture stream error: {err}");
    };

    let stream = match sample_format {
        cpal::SampleFormat::F32 => {
            let tx = tx.clone();
            device.build_input_stream(
                config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let mono = downmix(data, channels);
                    let _ = tx.try_send(mono);
                },
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let tx = tx.clone();
            device.build_input_stream(
                config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    let f32_data: Vec<f32> =
                        data.iter().map(|&s| s as f32 / 32768.0).collect();
                    let mono = downmix(&f32_data, channels);
                    let _ = tx.try_send(mono);
                },
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let tx = tx.clone();
            device.build_input_stream(
                config,
                move |data: &[u16], _: &cpal::InputCallbackInfo| {
                    let f32_data: Vec<f32> =
                        data.iter().map(|&s| (s as f32 - 32768.0) / 32768.0).collect();
                    let mono = downmix(&f32_data, channels);
                    let _ = tx.try_send(mono);
                },
                err_fn,
                None,
            )
        }
        other => {
            return Err(VoiceError::Other(format!(
                "unsupported audio sample format: {other:?}"
            )));
        }
    }
    .map_err(|e| VoiceError::Other(format!("failed to build input stream: {e}")))?;

    stream
        .play()
        .map_err(|e| VoiceError::Other(format!("failed to start input stream: {e}")))?;

    Ok(AudioCapture {
        _stream: stream,
        device_name: device_name_str,
        sample_rate,
        channels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_downmix_empty() {
        assert_eq!(downmix(&[], 1), Vec::<f32>::new());
        assert_eq!(downmix(&[], 2), Vec::<f32>::new());
        assert_eq!(downmix(&[1.0, 2.0], 0), Vec::<f32>::new());
    }

    #[test]
    fn test_downmix_mono() {
        let samples = vec![0.1, -0.5, 0.8, -0.2];
        assert_eq!(downmix(&samples, 1), samples);
    }

    #[test]
    fn test_downmix_stereo() {
        let stereo = vec![1.0, 0.5, -0.5, 0.5, 0.2, 0.4];
        let expected = [0.75, 0.0, 0.3];
        let result = downmix(&stereo, 2);
        assert_eq!(result.len(), expected.len());
        for (a, b) in result.iter().zip(expected.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn test_downmix_multichannel() {
        let quad = vec![0.2, 0.4, 0.6, 0.8]; // sum = 2.0, avg = 0.5
        let result = downmix(&quad, 4);
        assert_eq!(result.len(), 1);
        assert!((result[0] - 0.5).abs() < 1e-6);
    }
}

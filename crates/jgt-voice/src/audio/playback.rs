use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::broadcast;

use crate::error::VoiceError;
use crate::tts::Pcm;

/// Internal audio queue managing resampling, channel mixing, and non-blocking sample feeding.
pub struct AudioQueue {
    target_sample_rate: u32,
    target_channels: u16,
    samples: VecDeque<f32>,
    is_playing: bool,
    idle_sender: Option<broadcast::Sender<()>>,
}

impl AudioQueue {
    /// Creates a new audio queue targeting the given sample rate and channel count.
    pub fn new(target_sample_rate: u32, target_channels: u16) -> Self {
        Self {
            target_sample_rate,
            target_channels,
            samples: VecDeque::new(),
            is_playing: false,
            idle_sender: None,
        }
    }

    /// Sets a broadcast sender to notify when audio playback finishes and becomes idle.
    pub fn set_idle_sender(&mut self, sender: broadcast::Sender<()>) {
        self.idle_sender = Some(sender);
    }

    /// Returns the target sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.target_sample_rate
    }

    /// Returns the target channel count.
    pub fn channels(&self) -> u16 {
        self.target_channels
    }

    /// Returns the number of currently buffered audio samples.
    pub fn buffered_samples(&self) -> usize {
        self.samples.len()
    }

    /// Returns true if there are samples playing or queued.
    pub fn is_playing(&self) -> bool {
        self.is_playing && !self.samples.is_empty()
    }

    /// Enqueues a PCM buffer, converting to `f32`, resampling to target sample rate,
    /// and mixing to all target channels.
    pub fn enqueue(&mut self, pcm: Pcm) -> Result<(), VoiceError> {
        let f32_samples = pcm.to_f32();
        if f32_samples.is_empty() {
            return Ok(());
        }

        // Resample mono f32 to native output rate
        let resampled = crate::resample::resample(
            pcm.sample_rate,
            self.target_sample_rate,
            &f32_samples,
        )?;

        // Interleave across output channels
        let ch = self.target_channels.max(1) as usize;
        self.samples.reserve(resampled.len() * ch);

        if ch == 1 {
            self.samples.extend(resampled);
        } else {
            for sample in resampled {
                for _ in 0..ch {
                    self.samples.push_back(sample);
                }
            }
        }

        self.is_playing = true;
        Ok(())
    }

    /// Immediately drops all queued audio and resets playing status.
    ///
    /// Used for barge-in interruptions. Notifies idle listeners if playback was active.
    pub fn stop(&mut self) {
        let was_playing = self.is_playing;
        self.samples.clear();
        self.is_playing = false;

        if was_playing
            && let Some(ref tx) = self.idle_sender
        {
            let _ = tx.send(());
        }
    }

    /// Reads up to `output.len()` samples into the output slice.
    ///
    /// Fills unread slots with silence (`0.0`).
    /// If playback drains all samples, sets `is_playing = false` and fires idle notification.
    pub fn read_samples(&mut self, output: &mut [f32]) -> usize {
        let read_count = output.len().min(self.samples.len());
        for slot in output.iter_mut().take(read_count) {
            *slot = self.samples.pop_front().unwrap_or(0.0);
        }

        if read_count < output.len() {
            output[read_count..].fill(0.0);
        }

        if self.is_playing && self.samples.is_empty() {
            self.is_playing = false;
            if let Some(ref tx) = self.idle_sender {
                let _ = tx.send(());
            }
        }

        read_count
    }
}

/// Active audio player driving cpal playback stream.
pub struct Player {
    queue: Arc<Mutex<AudioQueue>>,
    idle_tx: broadcast::Sender<()>,
    _stream: Option<cpal::Stream>,
    device_name: String,
    sample_rate: u32,
    channels: u16,
}

impl Player {
    /// Returns the output playback device name.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Starts the audio player with the default output device (or named device).
    pub fn start(device_name: Option<&str>) -> Result<Self, VoiceError> {
        let host = cpal::default_host();
        let device = match device_name {
            Some(name) => {
                let devices: Vec<cpal::Device> = host
                    .output_devices()
                    .map_err(|e| VoiceError::Other(format!("failed to query output devices: {e}")))?
                    .collect();
                super::select_device(devices, name)?
            }
            None => host
                .default_output_device()
                .ok_or_else(|| VoiceError::Other("no default output audio device found".into()))?,
        };

        let device_name_str = super::get_device_name(&device);

        let default_config = device
            .default_output_config()
            .map_err(|e| VoiceError::Other(format!("failed to get default output config: {e}")))?;

        let sample_rate = default_config.sample_rate();
        let channels = default_config.channels();
        let sample_format = default_config.sample_format();
        let config: cpal::StreamConfig = default_config.config();

        tracing::info!(
            device = %device_name_str,
            sample_rate,
            channels,
            "output audio playback opened"
        );

        let (idle_tx, _) = broadcast::channel(16);
        let mut audio_queue = AudioQueue::new(sample_rate, channels);
        audio_queue.set_idle_sender(idle_tx.clone());
        let queue = Arc::new(Mutex::new(audio_queue));

        let err_fn = |err| {
            tracing::warn!("audio playback stream error: {err}");
        };

        let queue_clone = queue.clone();
        let stream = match sample_format {
            cpal::SampleFormat::F32 => {
                device.build_output_stream(
                    config,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        if let Ok(mut guard) = queue_clone.try_lock() {
                            guard.read_samples(data);
                        } else {
                            data.fill(0.0);
                        }
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::I16 => {
                let mut scratch = Vec::new();
                device.build_output_stream(
                    config,
                    move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                        scratch.resize(data.len(), 0.0);
                        if let Ok(mut guard) = queue_clone.try_lock() {
                            guard.read_samples(&mut scratch);
                            for (out, &s) in data.iter_mut().zip(scratch.iter()) {
                                *out = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                            }
                        } else {
                            data.fill(0);
                        }
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::U16 => {
                let mut scratch = Vec::new();
                device.build_output_stream(
                    config,
                    move |data: &mut [u16], _: &cpal::OutputCallbackInfo| {
                        scratch.resize(data.len(), 0.0);
                        if let Ok(mut guard) = queue_clone.try_lock() {
                            guard.read_samples(&mut scratch);
                            for (out, &s) in data.iter_mut().zip(scratch.iter()) {
                                *out = ((s.clamp(-1.0, 1.0) * 32767.0) + 32768.0) as u16;
                            }
                        } else {
                            data.fill(32768);
                        }
                    },
                    err_fn,
                    None,
                )
            }
            other => {
                return Err(VoiceError::Other(format!(
                    "unsupported audio output format: {other:?}"
                )));
            }
        }
        .map_err(|e| VoiceError::Other(format!("failed to build output stream: {e}")))?;

        stream
            .play()
            .map_err(|e| VoiceError::Other(format!("failed to start output stream: {e}")))?;

        Ok(Self {
            queue,
            idle_tx,
            _stream: Some(stream),
            device_name: device_name_str,
            sample_rate,
            channels,
        })
    }

    /// Creates a headless player instance without hardware CPAL audio device (for tests/mocking).
    pub fn headless(sample_rate: u32, channels: u16) -> Self {
        let (idle_tx, _) = broadcast::channel(16);
        let mut audio_queue = AudioQueue::new(sample_rate, channels);
        audio_queue.set_idle_sender(idle_tx.clone());
        let queue = Arc::new(Mutex::new(audio_queue));

        Self {
            queue,
            idle_tx,
            _stream: None,
            device_name: "headless".into(),
            sample_rate,
            channels,
        }
    }

    /// Enqueues a PCM audio buffer.
    pub fn enqueue(&self, pcm: Pcm) -> Result<(), VoiceError> {
        let mut guard = self
            .queue
            .lock()
            .map_err(|_| VoiceError::Other("audio queue lock poisoned".into()))?;
        guard.enqueue(pcm)
    }

    /// Instantly stops audio playback and drops queued audio.
    pub fn stop(&self) {
        if let Ok(mut guard) = self.queue.lock() {
            guard.stop();
        }
    }

    /// Returns true if audio is currently playing or queued.
    pub fn is_playing(&self) -> bool {
        self.queue
            .lock()
            .map(|q| q.is_playing())
            .unwrap_or(false)
    }

    /// Subscribes to playback idle notifications.
    pub fn subscribe_idle(&self) -> broadcast::Receiver<()> {
        self.idle_tx.subscribe()
    }

    /// Returns the native output sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Returns the output channel count.
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Exposes access to the underlying queue (for testing).
    pub fn queue(&self) -> Arc<Mutex<AudioQueue>> {
        self.queue.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    #[test]
    fn test_queue_upsampling_22050_to_48k_stereo() {
        // 1 second of 440 Hz sine wave at 22,050 Hz
        let rate_in = 22_050;
        let mut sine_22k = Vec::with_capacity(rate_in);
        for i in 0..rate_in {
            let t = i as f32 / rate_in as f32;
            let val = (2.0 * PI * 440.0 * t).sin();
            sine_22k.push((val * 32000.0) as i16);
        }

        let pcm = Pcm::new(sine_22k, rate_in as u32);

        // Queue configured for 48 kHz stereo (2 channels)
        let mut queue = AudioQueue::new(48_000, 2);
        assert!(!queue.is_playing());

        queue.enqueue(pcm).unwrap();
        assert!(queue.is_playing());

        // Expected output samples: ~48,000 * 2 channels = ~96,000 samples
        let total_samples = queue.buffered_samples();
        assert!(
            (total_samples as i32 - 96_000).abs() <= 200,
            "Expected ~96000 samples, got {total_samples}"
        );
    }

    #[test]
    fn test_queue_stop_and_barge_in() {
        let (idle_tx, mut idle_rx) = broadcast::channel(16);
        let mut queue = AudioQueue::new(48_000, 1);
        queue.set_idle_sender(idle_tx);

        let pcm = Pcm::new(vec![1000, 2000, 3000, 4000], 48_000);
        queue.enqueue(pcm).unwrap();
        assert!(queue.is_playing());
        assert_eq!(queue.buffered_samples(), 4);

        // Stop drops all audio immediately
        queue.stop();
        assert!(!queue.is_playing());
        assert_eq!(queue.buffered_samples(), 0);

        // Idle notification should have fired
        assert!(idle_rx.try_recv().is_ok());
    }

    #[test]
    fn test_queue_read_samples_and_idle_notification() {
        let (idle_tx, mut idle_rx) = broadcast::channel(16);
        let mut queue = AudioQueue::new(48_000, 1);
        queue.set_idle_sender(idle_tx);

        let pcm = Pcm::new(vec![16384, -16384], 48_000);
        queue.enqueue(pcm).unwrap();
        assert!(queue.is_playing());

        let mut buf = [0.0; 4];
        let read = queue.read_samples(&mut buf);
        assert_eq!(read, 2);
        assert!((buf[0] - 0.5).abs() < 1e-4);
        assert!((buf[1] - (-0.5)).abs() < 1e-4);
        assert_eq!(buf[2], 0.0);
        assert_eq!(buf[3], 0.0);

        // Queue drained: should no longer be playing and idle notification fires
        assert!(!queue.is_playing());
        assert!(idle_rx.try_recv().is_ok());
    }
}

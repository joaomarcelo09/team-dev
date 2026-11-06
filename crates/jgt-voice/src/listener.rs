use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::audio::capture::AudioCapture;
use crate::error::VoiceError;
use crate::resample::Resampler16k;
use crate::stt::{Stt, Transcriber};
use crate::vad::VadSegmenter;
use crate::VoiceEvent;

pub type SpeechCallback = Arc<dyn Fn(u32) + Send + Sync + 'static>;

/// Silence window to wait after a segment ends before emitting `UserSpeech` (~600 ms).
/// If another segment starts or arrives within this window, the transcripts are merged.
pub const MERGE_WINDOW: std::time::Duration = std::time::Duration::from_millis(600);

/// Internal command sent to the STT worker thread.
#[derive(Debug)]
pub enum SttCommand {
    Utterance {
        audio: Vec<f32>,
        silence_wait: std::time::Duration,
    },
    SpeechStarted,
    SpeechDropped,
    Cancel,
}

/// Handle to an active voice listener session.
pub struct ListenerHandle {
    muted: Arc<AtomicBool>,
    paused_for_tts: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    events: mpsc::Sender<VoiceEvent>,
    capture: Arc<Mutex<Option<AudioCapture>>>,
    vad_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    on_speech_cb: Arc<Mutex<Option<SpeechCallback>>>,
    stt_tx: Arc<Mutex<Option<std::sync::mpsc::Sender<SttCommand>>>>,
    sample_rate: u32,
    device_name: String,
}

impl ListenerHandle {
    /// Returns the capture device name.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Returns the capture sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Returns true if the listener is currently muted.
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::SeqCst)
    }

    /// Returns true if the listener is currently paused for TTS (half-duplex).
    pub fn is_paused_for_tts(&self) -> bool {
        self.paused_for_tts.load(Ordering::SeqCst)
    }

    /// Sets a callback invoked when continuous speech is detected in VAD.
    ///
    /// The callback receives the continuous speech duration in milliseconds.
    pub fn set_on_speech<F>(&self, cb: F)
    where
        F: Fn(u32) + Send + Sync + 'static,
    {
        if let Ok(mut guard) = self.on_speech_cb.lock() {
            *guard = Some(Arc::new(cb));
        }
    }

    /// Simulates speech activity for testing barge-in without audio hardware.
    pub fn simulate_speech(&self, speech_ms: u32) {
        if let Ok(guard) = self.on_speech_cb.lock()
            && let Some(ref cb) = *guard
        {
            cb(speech_ms);
        }
    }

    /// Emits a voice event to the events channel.
    pub fn send_event(&self, event: VoiceEvent) {
        let events = self.events.clone();
        if let Err(mpsc::error::TrySendError::Full(ev)) = events.try_send(event) {
            tokio::spawn(async move {
                let _ = events.send(ev).await;
            });
        }
    }

    /// Controls whether the listener is muted.
    ///
    /// When muted, audio is discarded before VAD, new speech capture is stopped (in-flight or held
    /// transcripts are preserved), and a `VoiceEvent::Muted` event is emitted.
    /// When unmuted, emits `VoiceEvent::Listening`.
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::SeqCst);
        let event = if muted {
            VoiceEvent::Muted
        } else {
            VoiceEvent::Listening
        };
        let events = self.events.clone();
        if let Err(mpsc::error::TrySendError::Full(ev)) = events.try_send(event) {
            tokio::spawn(async move {
                let _ = events.send(ev).await;
            });
        }
    }

    /// Pauses listening while TTS is speaking (half-duplex gate).
    ///
    /// Audio is discarded before VAD with no events emitted.
    pub fn pause_for_tts(&self, paused: bool) {
        self.paused_for_tts.store(paused, Ordering::SeqCst);
    }

    /// Shuts down all listener stages, stopping audio capture, VAD and STT, and cancelling any held transcript.
    pub fn stop(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }

        if let Ok(guard) = self.stt_tx.lock()
            && let Some(ref tx) = *guard
        {
            let _ = tx.send(SttCommand::Cancel);
        }

        if let Ok(mut cap_guard) = self.capture.lock() {
            cap_guard.take();
        }

        if let Some(task) = self.vad_task.lock().ok().and_then(|mut guard| guard.take()) {
            task.abort();
        }
    }

    /// Creates a mock `ListenerHandle` for testing control routing without audio hardware.
    pub fn mock(events: mpsc::Sender<VoiceEvent>) -> Self {
        let _ = events.try_send(VoiceEvent::Listening);
        Self {
            muted: Arc::new(AtomicBool::new(false)),
            paused_for_tts: Arc::new(AtomicBool::new(false)),
            stopped: Arc::new(AtomicBool::new(false)),
            events,
            capture: Arc::new(Mutex::new(None)),
            vad_task: Arc::new(Mutex::new(None)),
            on_speech_cb: Arc::new(Mutex::new(None)),
            stt_tx: Arc::new(Mutex::new(None)),
            sample_rate: 16000,
            device_name: "mock".into(),
        }
    }
}

impl Drop for ListenerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Orchestrates the 3-stage voice listening pipeline:
/// Capture -> Resample/VAD -> STT on a blocking thread.
pub struct Listener;

impl Listener {
    /// Starts the voice listener using the given configuration and event sender.
    ///
    /// Connects three stages:
    /// 1. Audio capture from microphone (`cpal`)
    /// 2. Resampling and VAD segmentation (`rubato` + `earshot`)
    /// 3. Speech-to-text inference (`whisper-rs`) on a blocking thread
    pub fn start(
        cfg: &jgt_core::config::Voice,
        events: mpsc::Sender<VoiceEvent>,
    ) -> Result<ListenerHandle, VoiceError> {
        let model_path = match crate::models::check_whisper(&cfg.stt_model) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, model = %cfg.stt_model, "whisper model check failed");
                return Err(e);
            }
        };
        let stt = match Stt::load(&model_path, cfg.threads) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, model_path = %model_path.display(), "failed to load whisper STT model");
                return Err(e);
            }
        };

        let (capture_tx, capture_rx) = mpsc::channel(64);
        let capture = match crate::audio::capture::start_capture(
            cfg.input_device.as_deref(),
            capture_tx,
        ) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, input_device = ?cfg.input_device, "failed to start audio capture");
                return Err(e);
            }
        };

        let sample_rate = capture.sample_rate();
        Self::start_pipeline(Some(capture), sample_rate, capture_rx, stt, events)
    }

    /// Starts the listener pipeline with an explicit audio stream or preconfigured capture receiver.
    pub fn start_pipeline(
        capture: Option<AudioCapture>,
        sample_rate: u32,
        capture_rx: mpsc::Receiver<Vec<f32>>,
        stt: Stt,
        events: mpsc::Sender<VoiceEvent>,
    ) -> Result<ListenerHandle, VoiceError> {
        Self::start_pipeline_with_transcriber(
            capture,
            sample_rate,
            capture_rx,
            Arc::new(stt),
            events,
        )
    }

    /// Starts the listener pipeline with a custom or mock speech transcriber.
    pub fn start_pipeline_with_transcriber(
        capture: Option<AudioCapture>,
        sample_rate: u32,
        mut capture_rx: mpsc::Receiver<Vec<f32>>,
        transcriber: Arc<dyn Transcriber>,
        events: mpsc::Sender<VoiceEvent>,
    ) -> Result<ListenerHandle, VoiceError> {
        let device_name = capture
            .as_ref()
            .map(|c| c.device_name().to_string())
            .unwrap_or_else(|| "default".into());

        let muted = Arc::new(AtomicBool::new(false));
        let paused_for_tts = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let on_speech_cb: Arc<Mutex<Option<SpeechCallback>>> = Arc::new(Mutex::new(None));

        let (stt_tx, stt_rx) = std::sync::mpsc::channel::<SttCommand>();

        // Stage 2: Resample & VAD
        let muted_clone = muted.clone();
        let paused_clone = paused_for_tts.clone();
        let stopped_clone = stopped.clone();
        let on_speech_cb_clone = on_speech_cb.clone();
        let stt_tx_vad = stt_tx.clone();
        let events_vad = events.clone();

        let vad_task = tokio::spawn(async move {
            let session_start = std::time::Instant::now();
            let mut resampler = match Resampler16k::new(sample_rate) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("failed to create resampler: {e}");
                    return;
                }
            };
            let mut vad = VadSegmenter::new();
            let mut first_frame_received = false;
            let mut no_audio_warned = false;
            let mut speech_start_time: Option<std::time::Instant> = None;

            // Wait up to 3 seconds for the first audio frame from capture.
            // If no frame arrives within 3 s, warn and emit VoiceEvent::NoAudio.
            let initial_chunk = tokio::select! {
                chunk = capture_rx.recv() => {
                    if let Some(ref c) = chunk {
                        first_frame_received = true;
                        let (rms, peak) = audio_levels(c);
                        let elapsed_ms = session_start.elapsed().as_millis() as u64;
                        tracing::info!(
                            elapsed_ms,
                            rms,
                            peak,
                            "first audio frame received from microphone"
                        );
                    }
                    chunk
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(3)) => {
                    tracing::warn!(
                        elapsed_ms = session_start.elapsed().as_millis() as u64,
                        "no audio frame received from microphone within 3 s of start"
                    );
                    let _ = events_vad.try_send(VoiceEvent::NoAudio);
                    no_audio_warned = true;
                    None
                }
            };

            let mut next_chunk = initial_chunk;

            while !stopped_clone.load(Ordering::SeqCst) {
                let chunk = match next_chunk.take() {
                    Some(c) => c,
                    None => match capture_rx.recv().await {
                        Some(c) => {
                            if !first_frame_received {
                                first_frame_received = true;
                                let (rms, peak) = audio_levels(&c);
                                let elapsed_ms = session_start.elapsed().as_millis() as u64;
                                tracing::info!(
                                    elapsed_ms,
                                    rms,
                                    peak,
                                    "first audio frame received from microphone"
                                );
                                if no_audio_warned {
                                    let ev = if muted_clone.load(Ordering::SeqCst) {
                                        VoiceEvent::Muted
                                    } else {
                                        VoiceEvent::Listening
                                    };
                                    let _ = events_vad.try_send(ev);
                                }
                            }
                            c
                        }
                        None => break,
                    },
                };

                if muted_clone.load(Ordering::SeqCst) {
                    if vad.is_speaking() {
                        let maybe_utterance = vad.finish();
                        if let Some(utterance) = maybe_utterance {
                            let duration_ms = speech_start_time
                                .take()
                                .map(|t| t.elapsed().as_millis() as u64)
                                .unwrap_or_else(|| (utterance.len() as u64 * 1000) / 16000);
                            tracing::info!(
                                duration_ms,
                                samples = utterance.len(),
                                "VAD speech end"
                            );
                            let _ = events_vad.try_send(VoiceEvent::Transcribing);
                            let silence_wait =
                                std::time::Duration::from_millis(vad.last_silence_wait_ms() as u64);
                            if stt_tx_vad
                                .send(SttCommand::Utterance {
                                    audio: utterance,
                                    silence_wait,
                                })
                                .is_err()
                            {
                                break;
                            }
                        } else {
                            speech_start_time = None;
                            let _ = stt_tx_vad.send(SttCommand::SpeechDropped);
                        }
                    } else {
                        vad.reset();
                        speech_start_time = None;
                    }
                    continue;
                }

                if paused_clone.load(Ordering::SeqCst) {
                    let was_speaking = vad.is_speaking();
                    vad.reset();
                    speech_start_time = None;
                    if was_speaking {
                        let _ = stt_tx_vad.send(SttCommand::SpeechDropped);
                    }
                    continue;
                }

                let resampled = match resampler.process(&chunk) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("resampling error in listener: {e}");
                        continue;
                    }
                };

                let was_speaking = vad.is_speaking();
                let on_speech_cb = on_speech_cb_clone.clone();
                let maybe_utterance = vad.push_with_frame_callback(&resampled, |ms| {
                    if ms > 0
                        && let Ok(guard) = on_speech_cb.lock()
                        && let Some(ref cb) = *guard
                    {
                        cb(ms);
                    }
                });

                if let Some(utterance) = maybe_utterance {
                    let duration_ms = speech_start_time
                        .take()
                        .map(|t| t.elapsed().as_millis() as u64)
                        .unwrap_or_else(|| (utterance.len() as u64 * 1000) / 16000);
                    tracing::info!(duration_ms, samples = utterance.len(), "VAD speech end");
                    let _ = events_vad.try_send(VoiceEvent::Transcribing);
                    let silence_wait =
                        std::time::Duration::from_millis(vad.last_silence_wait_ms() as u64);
                    if stt_tx_vad
                        .send(SttCommand::Utterance {
                            audio: utterance,
                            silence_wait,
                        })
                        .is_err()
                    {
                        break;
                    }
                } else if was_speaking && !vad.is_speaking() {
                    speech_start_time = None;
                    let _ = stt_tx_vad.send(SttCommand::SpeechDropped);
                    let _ = events_vad.try_send(VoiceEvent::Listening);
                }

                if !was_speaking && vad.is_speaking() {
                    speech_start_time = Some(std::time::Instant::now());
                    let elapsed_ms = session_start.elapsed().as_millis() as u64;
                    tracing::info!(elapsed_ms, "VAD speech start");
                    let _ = events_vad.try_send(VoiceEvent::Hearing);
                    let _ = stt_tx_vad.send(SttCommand::SpeechStarted);
                }
            }
        });

        // Stage 3: STT on a blocking thread with transcript merging
        let events_clone = events.clone();
        let stopped_thread = stopped.clone();
        let muted_thread = muted.clone();

        std::thread::Builder::new()
            .name("jgt-voice-stt".into())
            .spawn(move || {
                run_stt_loop(
                    stt_rx,
                    transcriber,
                    events_clone,
                    stopped_thread,
                    muted_thread,
                );
            })
            .map_err(|e| VoiceError::Other(format!("failed to spawn STT thread: {e}")))?;

        // Emit initial Listening state
        let _ = events.try_send(VoiceEvent::Listening);

        Ok(ListenerHandle {
            muted,
            paused_for_tts,
            stopped,
            events,
            capture: Arc::new(Mutex::new(capture)),
            vad_task: Arc::new(Mutex::new(Some(vad_task))),
            on_speech_cb,
            stt_tx: Arc::new(Mutex::new(Some(stt_tx))),
            sample_rate,
            device_name,
        })
    }
}

/// Computes the RMS (root mean square) and peak amplitude of an audio buffer.
pub fn audio_levels(audio: &[f32]) -> (f32, f32) {
    if audio.is_empty() {
        return (0.0, 0.0);
    }
    let mut sum_sq = 0.0f64;
    let mut peak = 0.0f32;
    for &sample in audio {
        let abs = sample.abs();
        if abs > peak {
            peak = abs;
        }
        sum_sq += (sample as f64) * (sample as f64);
    }
    let rms = (sum_sq / audio.len() as f64).sqrt() as f32;
    (rms, peak)
}

/// Runs the STT processing and transcript merging loop on a blocking thread.
///
/// After a segment ends, if another segment starts or arrives within `MERGE_WINDOW` (~600 ms),
/// consecutive transcripts are joined with a space and emitted as one `VoiceEvent::UserSpeech`.
/// End call cancels any held text, while mute only stops capturing new speech (queued and held utterances are preserved).
pub fn run_stt_loop(
    stt_rx: std::sync::mpsc::Receiver<SttCommand>,
    transcriber: Arc<dyn Transcriber>,
    events: mpsc::Sender<VoiceEvent>,
    stopped: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
) {
    let mut held_text: Option<String> = None;
    let mut merge_start: Option<std::time::Instant> = None;
    let mut speech_started_in_window = false;
    let mut deadline: Option<std::time::Instant> = None;

    while !stopped.load(Ordering::SeqCst) {
        let timeout = if held_text.is_some() && !speech_started_in_window {
            let now = std::time::Instant::now();
            let dl = deadline.unwrap_or_else(|| now + MERGE_WINDOW);
            Some(dl.saturating_duration_since(now))
        } else if speech_started_in_window {
            // Speech started in window: wait up to 10 seconds for the next segment to finish
            Some(std::time::Duration::from_secs(10))
        } else {
            None
        };

        let msg = match timeout {
            Some(to) => match stt_rx.recv_timeout(to) {
                Ok(cmd) => Some(cmd),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if speech_started_in_window {
                        // Safety timeout expired while waiting for speech
                        speech_started_in_window = false;
                    }
                    if !stopped.load(Ordering::SeqCst) {
                        if let Some(text) = held_text.take() {
                            let elapsed_ms = merge_start
                                .take()
                                .map(|s| s.elapsed().as_millis() as u64)
                                .unwrap_or(0);
                            tracing::info!(
                                elapsed_ms,
                                text = %text,
                                "merge flush -> UserSpeech sent"
                            );
                            let _ = events.blocking_send(VoiceEvent::UserSpeech(text));
                        }
                    } else {
                        held_text = None;
                        merge_start = None;
                    }
                    deadline = None;
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match stt_rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(cmd) => Some(cmd),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            },
        };

        let Some(cmd) = msg else { continue };

        if stopped.load(Ordering::SeqCst) {
            held_text = None;
            merge_start = None;
            speech_started_in_window = false;
            deadline = None;
        }

        match cmd {
            SttCommand::Cancel => {
                held_text = None;
                merge_start = None;
                speech_started_in_window = false;
                deadline = None;
            }
            SttCommand::SpeechStarted => {
                if held_text.is_some() {
                    speech_started_in_window = true;
                }
            }
            SttCommand::SpeechDropped => {
                speech_started_in_window = false;
                if held_text.is_some()
                    && !stopped.load(Ordering::SeqCst)
                {
                    deadline = Some(std::time::Instant::now() + MERGE_WINDOW);
                } else {
                    deadline = None;
                }
            }
            SttCommand::Utterance {
                audio,
                silence_wait,
            } => {
                if stopped.load(Ordering::SeqCst) {
                    held_text = None;
                    merge_start = None;
                    speech_started_in_window = false;
                    deadline = None;
                    continue;
                }

                let (rms, peak) = audio_levels(&audio);
                tracing::debug!(
                    rms,
                    peak,
                    samples = audio.len(),
                    "utterance audio level before transcription: rms={:.4}, peak={:.4}",
                    rms,
                    peak
                );

                tracing::info!(samples = audio.len(), "STT start");
                let start = std::time::Instant::now();
                match transcriber.transcribe(&audio) {
                    Ok(text) => {
                        let transcription_duration = start.elapsed();
                        let elapsed_ms = transcription_duration.as_millis() as u64;
                        tracing::info!(
                            elapsed_ms,
                            silence_wait_ms = silence_wait.as_millis() as u64,
                            text = %text,
                            "STT finish"
                        );

                        speech_started_in_window = false;

                        if !text.is_empty() {
                            if merge_start.is_none() {
                                merge_start = Some(std::time::Instant::now());
                            }
                            held_text = match held_text.take() {
                                Some(prev) => Some(format!("{prev} {text}")),
                                None => Some(text),
                            };
                            deadline = Some(std::time::Instant::now() + MERGE_WINDOW);
                        } else if held_text.is_some() && deadline.is_none() {
                            deadline = Some(std::time::Instant::now() + MERGE_WINDOW);
                        } else if held_text.is_none() {
                            let ev = if muted.load(Ordering::SeqCst) {
                                VoiceEvent::Muted
                            } else {
                                VoiceEvent::Listening
                            };
                            let _ = events.blocking_send(ev);
                        }
                    }
                    Err(err) => {
                        let elapsed_ms = start.elapsed().as_millis() as u64;
                        tracing::error!(elapsed_ms, error = %err, "STT error: {err}");
                        held_text = None;
                        merge_start = None;
                        speech_started_in_window = false;
                        deadline = None;
                        let _ = events.blocking_send(VoiceEvent::Error);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockTranscriber;

    impl Transcriber for MockTranscriber {
        fn transcribe(&self, audio: &[f32]) -> Result<String, VoiceError> {
            if audio.is_empty() {
                return Ok(String::new());
            }
            match audio[0] as i32 {
                1 => Ok("first".to_string()),
                2 => Ok("second".to_string()),
                3 => Ok("third".to_string()),
                _ => Ok("speech".to_string()),
            }
        }
    }

    #[tokio::test]
    async fn test_merge_window_two_segments_within_window_sent_as_one() {
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let (stt_tx, stt_rx) = std::sync::mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let transcriber = Arc::new(MockTranscriber);

        let stopped_clone = stopped.clone();
        let muted_clone = muted.clone();
        let thread_handle = std::thread::spawn(move || {
            run_stt_loop(stt_rx, transcriber, events_tx, stopped_clone, muted_clone);
        });

        // Send segment 1
        stt_tx
            .send(SttCommand::Utterance {
                audio: vec![1.0; 1600],
                silence_wait: std::time::Duration::from_millis(1120),
            })
            .unwrap();

        // Send segment 2 within merge window (150 ms < 600 ms)
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        stt_tx
            .send(SttCommand::Utterance {
                audio: vec![2.0; 1600],
                silence_wait: std::time::Duration::from_millis(1120),
            })
            .unwrap();

        // After merge window expires, exactly ONE merged UserSpeech should be received
        let ev = tokio::time::timeout(std::time::Duration::from_millis(1500), events_rx.recv())
            .await
            .expect("timeout waiting for merged speech")
            .unwrap();

        assert_eq!(ev, VoiceEvent::UserSpeech("first second".to_string()));

        // Ensure no subsequent event arrives
        assert!(events_rx.try_recv().is_err());

        stopped.store(true, Ordering::SeqCst);
        let _ = stt_tx.send(SttCommand::Cancel);
        let _ = thread_handle.join();
    }


    #[tokio::test]
    async fn test_merge_window_gap_longer_than_window_sends_two() {
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let (stt_tx, stt_rx) = std::sync::mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let transcriber = Arc::new(MockTranscriber);

        let stopped_clone = stopped.clone();
        let muted_clone = muted.clone();
        let thread_handle = std::thread::spawn(move || {
            run_stt_loop(stt_rx, transcriber, events_tx, stopped_clone, muted_clone);
        });

        // Send segment 1
        stt_tx
            .send(SttCommand::Utterance {
                audio: vec![1.0; 1600],
                silence_wait: std::time::Duration::from_millis(1120),
            })
            .unwrap();

        // Wait longer than merge window (> 600 ms)
        let ev1 = tokio::time::timeout(std::time::Duration::from_millis(1200), events_rx.recv())
            .await
            .expect("timeout waiting for first speech")
            .unwrap();
        assert_eq!(ev1, VoiceEvent::UserSpeech("first".to_string()));

        // Send segment 2 after the window has expired
        stt_tx
            .send(SttCommand::Utterance {
                audio: vec![2.0; 1600],
                silence_wait: std::time::Duration::from_millis(1120),
            })
            .unwrap();

        let ev2 = tokio::time::timeout(std::time::Duration::from_millis(1200), events_rx.recv())
            .await
            .expect("timeout waiting for second speech")
            .unwrap();
        assert_eq!(ev2, VoiceEvent::UserSpeech("second".to_string()));

        stopped.store(true, Ordering::SeqCst);
        let _ = stt_tx.send(SttCommand::Cancel);
        let _ = thread_handle.join();
    }


    #[tokio::test]
    async fn test_merge_window_cancel_discards_held_text() {
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let (stt_tx, stt_rx) = std::sync::mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let transcriber = Arc::new(MockTranscriber);

        let stopped_clone = stopped.clone();
        let muted_clone = muted.clone();
        let thread_handle = std::thread::spawn(move || {
            run_stt_loop(stt_rx, transcriber, events_tx, stopped_clone, muted_clone);
        });

        // Send segment 1
        stt_tx
            .send(SttCommand::Utterance {
                audio: vec![1.0; 1600],
                silence_wait: std::time::Duration::from_millis(1120),
            })
            .unwrap();

        // Within 100 ms, Cancel is sent (e.g. End call)
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        stt_tx.send(SttCommand::Cancel).unwrap();

        // Wait past merge window
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        // Held text should have been discarded, nothing emitted
        assert!(events_rx.try_recv().is_err());

        stopped.store(true, Ordering::SeqCst);
        drop(stt_tx);
        let _ = thread_handle.join();
    }

    #[tokio::test]
    async fn test_merge_window_speech_started_in_window_extends_and_merges() {
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let (stt_tx, stt_rx) = std::sync::mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let transcriber = Arc::new(MockTranscriber);

        let stopped_clone = stopped.clone();
        let muted_clone = muted.clone();
        let thread_handle = std::thread::spawn(move || {
            run_stt_loop(stt_rx, transcriber, events_tx, stopped_clone, muted_clone);
        });

        // Send segment 1
        stt_tx
            .send(SttCommand::Utterance {
                audio: vec![1.0; 1600],
                silence_wait: std::time::Duration::from_millis(1120),
            })
            .unwrap();

        // At 200 ms (< 600 ms), speech starts
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        stt_tx.send(SttCommand::SpeechStarted).unwrap();

        // At 700 ms (> 600 ms from segment 1), verify UserSpeech is NOT emitted yet because speech is ongoing
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(events_rx.try_recv().is_err());

        // Segment 2 completes
        stt_tx
            .send(SttCommand::Utterance {
                audio: vec![2.0; 1600],
                silence_wait: std::time::Duration::from_millis(1120),
            })
            .unwrap();

        // Now merge window expires from segment 2
        let ev = tokio::time::timeout(std::time::Duration::from_millis(1200), events_rx.recv())
            .await
            .expect("timeout waiting for merged speech")
            .unwrap();
        assert_eq!(ev, VoiceEvent::UserSpeech("first second".to_string()));

        stopped.store(true, Ordering::SeqCst);
        let _ = stt_tx.send(SttCommand::Cancel);
        let _ = thread_handle.join();
    }


    #[tokio::test]
    async fn test_listener_handle_mute_and_pause() {
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let muted = Arc::new(AtomicBool::new(false));
        let paused_for_tts = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));

        let handle = ListenerHandle {
            muted,
            paused_for_tts,
            stopped,
            events: events_tx,
            capture: Arc::new(Mutex::new(None)),
            vad_task: Arc::new(Mutex::new(None)),
            on_speech_cb: Arc::new(Mutex::new(None)),
            stt_tx: Arc::new(Mutex::new(None)),
            sample_rate: 16_000,
            device_name: "test".into(),
        };

        // Initially unmuted
        assert!(!handle.is_muted());
        assert!(!handle.is_paused_for_tts());

        // Set muted
        handle.set_muted(true);
        assert!(handle.is_muted());
        let ev = events_rx.recv().await.unwrap();
        assert_eq!(ev, VoiceEvent::Muted);

        // Unmute
        handle.set_muted(false);
        assert!(!handle.is_muted());
        let ev = events_rx.recv().await.unwrap();
        assert_eq!(ev, VoiceEvent::Listening);

        // Pause for TTS emits no events
        handle.pause_for_tts(true);
        assert!(handle.is_paused_for_tts());
        assert!(events_rx.try_recv().is_err());

        handle.pause_for_tts(false);
        assert!(!handle.is_paused_for_tts());
        assert!(events_rx.try_recv().is_err());

        handle.stop();
    }

    #[tokio::test]
    async fn test_merge_window_paused_for_tts_still_sends_user_speech() {
        let (capture_tx, capture_rx) = mpsc::channel(16);
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let transcriber = Arc::new(MockTranscriber);

        let handle = Listener::start_pipeline_with_transcriber(
            None,
            16_000,
            capture_rx,
            transcriber,
            events_tx,
        )
        .unwrap();

        // Consume initial Listening event
        let init_ev = events_rx.recv().await.unwrap();
        assert_eq!(init_ev, VoiceEvent::Listening);

        // Send an utterance to the STT loop
        let stt_tx = handle.stt_tx.lock().unwrap().as_ref().unwrap().clone();
        stt_tx
            .send(SttCommand::Utterance {
                audio: vec![1.0; 1600],
                silence_wait: std::time::Duration::from_millis(1120),
            })
            .unwrap();

        // Within merge window, pause for TTS
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        handle.pause_for_tts(true);

        // Deliver audio chunks to VAD task while paused
        capture_tx.send(vec![0.0f32; 512]).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Ensure held text was not cancelled by TTS pause: UserSpeech should still be sent
        let ev = tokio::time::timeout(std::time::Duration::from_millis(1500), events_rx.recv())
            .await
            .expect("timeout waiting for speech during tts pause")
            .unwrap();
        assert_eq!(ev, VoiceEvent::UserSpeech("first".to_string()));

        handle.stop();
    }

    #[tokio::test]
    async fn test_merge_window_speech_dropped_restarts_deadline() {
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let (stt_tx, stt_rx) = std::sync::mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let transcriber = Arc::new(MockTranscriber);

        let stopped_clone = stopped.clone();
        let muted_clone = muted.clone();
        let thread_handle = std::thread::spawn(move || {
            run_stt_loop(stt_rx, transcriber, events_tx, stopped_clone, muted_clone);
        });

        // Send segment 1
        stt_tx
            .send(SttCommand::Utterance {
                audio: vec![1.0; 1600],
                silence_wait: std::time::Duration::from_millis(1120),
            })
            .unwrap();

        // At 200 ms (< 600 ms), short speech starts
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        stt_tx.send(SttCommand::SpeechStarted).unwrap();

        // Short noise burst ends without utterance: SpeechDropped sent at 300 ms
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        stt_tx.send(SttCommand::SpeechDropped).unwrap();

        // Deadline restarted: verify speech is NOT emitted immediately (< 400 ms from dropped)
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(events_rx.try_recv().is_err());

        // But IS emitted around ~600 ms after SpeechDropped (well before the 10 s safety timeout)
        let ev = tokio::time::timeout(std::time::Duration::from_millis(1200), events_rx.recv())
            .await
            .expect("timeout waiting for speech after SpeechDropped (must not wait 10s)")
            .unwrap();
        assert_eq!(ev, VoiceEvent::UserSpeech("first".to_string()));

        stopped.store(true, Ordering::SeqCst);
        let _ = stt_tx.send(SttCommand::Cancel);
        let _ = thread_handle.join();
    }


    #[tokio::test]
    async fn test_merge_window_timeout_flush_preserves_held_text_when_muted() {
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let (stt_tx, stt_rx) = std::sync::mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let transcriber = Arc::new(MockTranscriber);

        let stopped_clone = stopped.clone();
        let muted_clone = muted.clone();
        let thread_handle = std::thread::spawn(move || {
            run_stt_loop(stt_rx, transcriber, events_tx, stopped_clone, muted_clone);
        });

        // Send segment 1
        stt_tx
            .send(SttCommand::Utterance {
                audio: vec![1.0; 1600],
                silence_wait: std::time::Duration::from_millis(1120),
            })
            .unwrap();

        // Mute flag is set before the timeout expires (without sending Cancel command)
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        muted.store(true, Ordering::SeqCst);

        // Wait past the merge window timeout
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        // Verify timeout flush sends held text even though muted is true
        let event = events_rx.recv().await.unwrap();
        assert_eq!(event, VoiceEvent::UserSpeech("first".to_string()));

        stopped.store(true, Ordering::SeqCst);
        let _ = stt_tx.send(SttCommand::Cancel);
        let _ = thread_handle.join();
    }

    #[tokio::test]
    async fn test_queued_utterance_transcribed_and_sent_after_mute() {
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let (stt_tx, stt_rx) = std::sync::mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let transcriber = Arc::new(MockTranscriber);

        let stopped_clone = stopped.clone();
        let muted_clone = muted.clone();
        let thread_handle = std::thread::spawn(move || {
            run_stt_loop(stt_rx, transcriber, events_tx, stopped_clone, muted_clone);
        });

        // Send utterance then immediately set muted
        stt_tx
            .send(SttCommand::Utterance {
                audio: vec![2.0; 1600],
                silence_wait: std::time::Duration::from_millis(1120),
            })
            .unwrap();
        muted.store(true, Ordering::SeqCst);

        // Wait for transcription and merge window flush
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

        let event = events_rx.recv().await.unwrap();
        assert_eq!(event, VoiceEvent::UserSpeech("second".to_string()));

        stopped.store(true, Ordering::SeqCst);
        let _ = stt_tx.send(SttCommand::Cancel);
        let _ = thread_handle.join();
    }

    #[tokio::test]
    async fn test_mute_during_speech_sends_the_utterance() {
        let (capture_tx, capture_rx) = mpsc::channel(64);
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let transcriber = Arc::new(MockTranscriber);

        let handle = Listener::start_pipeline_with_transcriber(
            None,
            16_000,
            capture_rx,
            transcriber,
            events_tx,
        )
        .unwrap();

        // Consume initial Listening event
        let init_ev = events_rx.recv().await.unwrap();
        assert_eq!(init_ev, VoiceEvent::Listening);

        // Notify when speech reaches >= 320 ms (> MIN_SPEECH_SAMPLES 304 ms)
        let (speech_done_tx, speech_done_rx) = tokio::sync::oneshot::channel();
        let speech_done_tx = Arc::new(Mutex::new(Some(speech_done_tx)));
        handle.set_on_speech(move |ms| {
            if ms >= 320 {
                if let Ok(mut guard) = speech_done_tx.lock() {
                    if let Some(tx) = guard.take() {
                        let _ = tx.send(());
                    }
                }
            }
        });

        // Generate speech frames using a 200 Hz tone that EarshotDetector scores > 0.7.
        // Send frames continuously in background until speech onset & duration is confirmed.
        let capture_tx_clone = capture_tx.clone();
        let speech_feed_task = tokio::spawn(async move {
            let speech_frame: Vec<f32> = (0..256)
                .map(|i| (i as f32 * 200.0 * 2.0 * std::f32::consts::PI / 16000.0).sin() * 0.5)
                .collect();
            for _ in 0..40 {
                if capture_tx_clone.send(speech_frame.clone()).await.is_err() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        });

        // Wait for Hearing event (speech confirmed, Speaking state active)
        let ev = tokio::time::timeout(std::time::Duration::from_millis(1000), events_rx.recv())
            .await
            .expect("timeout waiting for Hearing event")
            .unwrap();
        assert_eq!(ev, VoiceEvent::Hearing);

        // Wait until VAD has processed >= 320 ms of speech
        tokio::time::timeout(std::time::Duration::from_millis(1000), speech_done_rx)
            .await
            .expect("timeout waiting for speech to reach 320 ms")
            .unwrap();

        // Mute while speech is active
        handle.set_muted(true);
        assert!(handle.is_muted());

        let ev = events_rx.recv().await.unwrap();
        assert_eq!(ev, VoiceEvent::Muted);

        // Send a dummy chunk so the VAD loop processes the chunk and checks mute
        capture_tx.send(vec![0.0f32; 256]).await.unwrap();

        // Mute during speech must finish the VAD segment and send the utterance:
        // Transcribing event emitted when SttCommand::Utterance is sent.
        let ev = tokio::time::timeout(std::time::Duration::from_millis(1000), events_rx.recv())
            .await
            .expect("timeout waiting for Transcribing event")
            .unwrap();
        assert_eq!(ev, VoiceEvent::Transcribing);

        // Verify UserSpeech is received after merge window
        let ev = tokio::time::timeout(std::time::Duration::from_millis(1500), events_rx.recv())
            .await
            .expect("timeout waiting for UserSpeech")
            .unwrap();
        assert_eq!(ev, VoiceEvent::UserSpeech("speech".to_string()));

        let _ = speech_feed_task.await;
        handle.stop();
    }

    #[tokio::test]
    async fn test_mute_during_short_speech_held_text_not_stuck_on_safety_timeout() {
        let (capture_tx, capture_rx) = mpsc::channel(64);
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let transcriber = Arc::new(MockTranscriber);

        let handle = Listener::start_pipeline_with_transcriber(
            None,
            16_000,
            capture_rx,
            transcriber,
            events_tx,
        )
        .unwrap();

        // Consume initial Listening event
        let init_ev = events_rx.recv().await.unwrap();
        assert_eq!(init_ev, VoiceEvent::Listening);

        // Populate held_text via an utterance directly to the STT loop
        let stt_tx = handle.stt_tx.lock().unwrap().as_ref().unwrap().clone();
        stt_tx
            .send(SttCommand::Utterance {
                audio: vec![1.0; 1600],
                silence_wait: std::time::Duration::from_millis(1120),
            })
            .unwrap();

        // Notify right when speech onset triggers (160 ms), well before MIN_SPEECH_SAMPLES (304 ms)
        let (speech_started_tx, speech_started_rx) = tokio::sync::oneshot::channel();
        let speech_started_tx = Arc::new(Mutex::new(Some(speech_started_tx)));
        handle.set_on_speech(move |ms| {
            if ms >= 160 && ms <= 200 {
                if let Ok(mut guard) = speech_started_tx.lock() {
                    if let Some(tx) = guard.take() {
                        let _ = tx.send(());
                    }
                }
            }
        });

        // Feed speech in background until onset is detected
        let capture_tx_clone = capture_tx.clone();
        let speech_feed_task = tokio::spawn(async move {
            let speech_frame: Vec<f32> = (0..256)
                .map(|i| (i as f32 * 200.0 * 2.0 * std::f32::consts::PI / 16000.0).sin() * 0.5)
                .collect();
            for _ in 0..35 {
                if capture_tx_clone.send(speech_frame.clone()).await.is_err() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        });

        // Wait for onset (Hearing event)
        let ev = tokio::time::timeout(std::time::Duration::from_millis(1000), events_rx.recv())
            .await
            .expect("timeout waiting for Hearing event")
            .unwrap();
        assert_eq!(ev, VoiceEvent::Hearing);

        // Wait until onset callback confirms 160 ms
        tokio::time::timeout(std::time::Duration::from_millis(1000), speech_started_rx)
            .await
            .expect("timeout waiting for speech onset (160 ms)")
            .unwrap();

        // Mute immediately while speech is < MIN_SPEECH_SAMPLES (304 ms)
        handle.set_muted(true);
        let ev = events_rx.recv().await.unwrap();
        assert_eq!(ev, VoiceEvent::Muted);

        // Send a dummy chunk so the VAD loop processes the mute
        capture_tx.send(vec![0.0f32; 256]).await.unwrap();

        // Because speech < MIN_SPEECH_SAMPLES, no utterance is sent; instead SpeechDropped
        // is sent, resetting speech_started_in_window and restarting the merge window.
        // The held transcript ("first") must flush in ~600 ms, NOT waiting for the 10 s timeout!
        let ev = tokio::time::timeout(std::time::Duration::from_millis(1500), events_rx.recv())
            .await
            .expect("timeout waiting for held text flush (must not hang 10 s)")
            .unwrap();
        assert_eq!(ev, VoiceEvent::UserSpeech("first".to_string()));

        let _ = speech_feed_task.await;
        handle.stop();
    }

    #[tokio::test]
    async fn test_listener_handle_mute_does_not_cancel_stt() {
        let (events_tx, _events_rx) = mpsc::channel(16);
        let (stt_tx, stt_rx) = std::sync::mpsc::channel();
        let muted = Arc::new(AtomicBool::new(false));
        let paused_for_tts = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));

        let handle = ListenerHandle {
            muted,
            paused_for_tts,
            stopped,
            events: events_tx,
            capture: Arc::new(Mutex::new(None)),
            vad_task: Arc::new(Mutex::new(None)),
            on_speech_cb: Arc::new(Mutex::new(None)),
            stt_tx: Arc::new(Mutex::new(Some(stt_tx))),
            sample_rate: 16_000,
            device_name: "test".into(),
        };

        handle.set_muted(true);
        assert!(handle.is_muted());
        // Verify NO SttCommand::Cancel was sent
        assert!(stt_rx.try_recv().is_err());

        // But stop() (End call) DOES send Cancel
        handle.stop();
        assert!(matches!(stt_rx.try_recv(), Ok(SttCommand::Cancel)));
    }


    #[tokio::test]
    async fn test_merge_window_timeout_flush_respects_stopped() {
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let (stt_tx, stt_rx) = std::sync::mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let transcriber = Arc::new(MockTranscriber);

        let stopped_clone = stopped.clone();
        let muted_clone = muted.clone();
        let thread_handle = std::thread::spawn(move || {
            run_stt_loop(stt_rx, transcriber, events_tx, stopped_clone, muted_clone);
        });

        // Send segment 1
        stt_tx
            .send(SttCommand::Utterance {
                audio: vec![1.0; 1600],
                silence_wait: std::time::Duration::from_millis(1120),
            })
            .unwrap();

        // Stop flag is set before the timeout expires (without sending Cancel command)
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        stopped.store(true, Ordering::SeqCst);

        // Wait past the merge window timeout
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        // Verify timeout flush did not send held text because stopped is true
        assert!(events_rx.try_recv().is_err());

        let _ = thread_handle.join();
    }

    #[tokio::test]
    async fn test_merge_window_empty_utterance_does_not_delay_flush() {
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let (stt_tx, stt_rx) = std::sync::mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let transcriber = Arc::new(MockTranscriber);

        let stopped_clone = stopped.clone();
        let muted_clone = muted.clone();
        let thread_handle = std::thread::spawn(move || {
            run_stt_loop(stt_rx, transcriber, events_tx, stopped_clone, muted_clone);
        });

        // Send segment 1 (transcribes to "first")
        stt_tx
            .send(SttCommand::Utterance {
                audio: vec![1.0; 1600],
                silence_wait: std::time::Duration::from_millis(1120),
            })
            .unwrap();

        // At 200 ms, speech started
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        stt_tx.send(SttCommand::SpeechStarted).unwrap();

        // Utterance 2 arrives but transcribes to "" (empty audio in MockTranscriber returns "")
        stt_tx
            .send(SttCommand::Utterance {
                audio: vec![],
                silence_wait: std::time::Duration::from_millis(1120),
            })
            .unwrap();

        // Verify that held text ("first") flushes around ~600 ms, well before the 10 s safety timeout
        let ev = tokio::time::timeout(std::time::Duration::from_millis(1200), events_rx.recv())
            .await
            .expect("timeout waiting for speech to flush after empty utterance")
            .unwrap();
        assert_eq!(ev, VoiceEvent::UserSpeech("first".to_string()));

        stopped.store(true, Ordering::SeqCst);
        let _ = stt_tx.send(SttCommand::Cancel);
        let _ = thread_handle.join();
    }


    #[test]
    fn test_audio_levels() {
        assert_eq!(audio_levels(&[]), (0.0, 0.0));
        let silence = vec![0.0f32; 100];
        assert_eq!(audio_levels(&silence), (0.0, 0.0));
        let samples = vec![0.5f32, -0.5f32, 0.5f32, -0.5f32];
        let (rms, peak) = audio_levels(&samples);
        assert!((rms - 0.5).abs() < 1e-6);
        assert!((peak - 0.5).abs() < 1e-6);
    }

    #[test]
    fn test_listener_handle_accessors() {
        let (events_tx, _events_rx) = mpsc::channel(16);
        let mock = ListenerHandle::mock(events_tx);
        assert_eq!(mock.sample_rate(), 16000);
        assert_eq!(mock.device_name(), "mock");
    }

    #[tokio::test]
    async fn test_run_stt_loop_empty_transcript_emits_listening() {
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let (stt_tx, stt_rx) = std::sync::mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let transcriber = Arc::new(MockTranscriber);

        let stopped_clone = stopped.clone();
        let muted_clone = muted.clone();
        let thread_handle = std::thread::spawn(move || {
            run_stt_loop(stt_rx, transcriber, events_tx, stopped_clone, muted_clone);
        });

        // Send utterance that transcribes to empty text when nothing is held
        stt_tx
            .send(SttCommand::Utterance {
                audio: vec![],
                silence_wait: std::time::Duration::from_millis(1120),
            })
            .unwrap();

        // Verify that VoiceEvent::Listening is emitted so UI returns from Transcribing to Listening
        let ev = tokio::time::timeout(std::time::Duration::from_millis(1000), events_rx.recv())
            .await
            .expect("timeout waiting for Listening event after empty transcript")
            .unwrap();
        assert_eq!(ev, VoiceEvent::Listening);

        stopped.store(true, Ordering::SeqCst);
        let _ = stt_tx.send(SttCommand::Cancel);
        let _ = thread_handle.join();
    }
}

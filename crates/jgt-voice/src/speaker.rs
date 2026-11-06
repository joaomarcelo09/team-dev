use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::audio::playback::Player;
use crate::error::VoiceError;
use crate::tts::{split_sentences, PiperCommand, TtsEngine};
use crate::VoiceEvent;

pub type SpeakingCallback = Arc<dyn Fn(bool) + Send + Sync + 'static>;

/// Maximum number of pending speech requests queued before dropping the oldest.
pub const MAX_PENDING_SPEECH: usize = 16;

struct SpeakerState {
    queue: VecDeque<String>,
    worker_task: Option<tokio::task::JoinHandle<()>>,
    active_synth_task: Option<tokio::task::JoinHandle<()>>,
}

/// Handle to control active TTS speaking playback and register status callbacks.
pub struct SpeakerHandle {
    player: Arc<Player>,
    events: mpsc::Sender<VoiceEvent>,
    is_speaking: Arc<AtomicBool>,
    on_speaking_cb: Arc<Mutex<Option<SpeakingCallback>>>,
    state: Arc<Mutex<SpeakerState>>,
    notify: Arc<tokio::sync::Notify>,
    tts: Arc<dyn TtsEngine>,
}

impl SpeakerHandle {
    /// Returns true if currently synthesizing or playing speech.
    pub fn is_speaking(&self) -> bool {
        self.is_speaking.load(Ordering::SeqCst) || self.player.is_playing()
    }

    /// Exposes a reference to the active audio player (for queue inspections/testing).
    pub fn player(&self) -> Arc<Player> {
        self.player.clone()
    }

    /// Returns the number of pending speech requests in the queue.
    pub fn pending_count(&self) -> usize {
        self.state
            .lock()
            .map(|guard| guard.queue.len())
            .unwrap_or(0)
    }

    /// Sets a callback invoked with `true` when speaking starts and `false` when speaking finishes or is stopped.
    ///
    /// Used for half-duplex mic gating (`ListenerHandle::pause_for_tts`).
    pub fn set_on_speaking<F>(&self, cb: F)
    where
        F: Fn(bool) + Send + Sync + 'static,
    {
        if let Ok(mut guard) = self.on_speaking_cb.lock() {
            *guard = Some(Arc::new(cb));
        }
    }

    /// Speaks the given text.
    ///
    /// Requests are queued in a bounded FIFO queue and spoken sequentially in order.
    /// Sentences are split and synthesized individually, streaming PCM chunks to the player.
    /// Note: `speech_text` preprocessing is the caller's responsibility.
    pub fn speak(&self, text: &str) -> Result<(), VoiceError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(());
        }

        let mut state = self.state.lock().unwrap();
        if state.queue.len() >= MAX_PENDING_SPEECH {
            // Drop oldest pending request when bounded capacity is reached
            state.queue.pop_front();
        }
        state.queue.push_back(trimmed.to_string());

        let need_spawn = match &state.worker_task {
            Some(task) => task.is_finished(),
            None => true,
        };

        if need_spawn {
            let player = self.player.clone();
            let events = self.events.clone();
            let is_speaking = self.is_speaking.clone();
            let on_speaking_cb = self.on_speaking_cb.clone();
            let tts = self.tts.clone();
            let state_clone = self.state.clone();
            let notify = self.notify.clone();

            let task = tokio::spawn(async move {
                let mut started = false;
                let mut idle_rx = player.subscribe_idle();

                loop {
                    // 1. Pop next speech request from queue
                    let next_text = {
                        let mut guard = state_clone.lock().unwrap();
                        guard.queue.pop_front()
                    };

                    if let Some(text) = next_text {
                        let sentences = split_sentences(&text);
                        if sentences.is_empty() {
                            continue;
                        }

                        let (pcm_tx, mut pcm_rx) = mpsc::channel(4);
                        let tts_sentences = sentences.clone();
                        let tts_clone = tts.clone();

                        let synth_handle = tokio::spawn(async move {
                            for sentence in tts_sentences {
                                let tts_start = std::time::Instant::now();
                                tracing::info!(sentence = %sentence, "TTS start");
                                match tts_clone.synthesize(&sentence).await {
                                    Ok(pcm) => {
                                        let elapsed_ms = tts_start.elapsed().as_millis() as u64;
                                        tracing::info!(
                                            elapsed_ms,
                                            sentence = %sentence,
                                            samples = pcm.samples.len(),
                                            "TTS finish"
                                        );
                                        if pcm_tx.send(pcm).await.is_err() {
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        let elapsed_ms = tts_start.elapsed().as_millis() as u64;
                                        tracing::error!(
                                            elapsed_ms,
                                            error = %e,
                                            sentence = %sentence,
                                            "TTS synthesis error: {e}"
                                        );
                                        break;
                                    }
                                }
                            }
                        });

                        {
                            let mut guard = state_clone.lock().unwrap();
                            guard.active_synth_task = Some(synth_handle);
                        }

                        while let Some(pcm) = pcm_rx.recv().await {
                            if !started {
                                started = true;
                                is_speaking.store(true, Ordering::SeqCst);
                                let _ = events.try_send(VoiceEvent::Speaking);
                                if let Ok(guard) = on_speaking_cb.lock()
                                    && let Some(ref cb) = *guard
                                {
                                    cb(true);
                                }
                            }

                            if let Err(e) = player.enqueue(pcm) {
                                tracing::error!("Player enqueue error: {e}");
                                let _ = events.try_send(VoiceEvent::Error);
                                break;
                            }
                        }

                        let synth_task = {
                            let mut guard = state_clone.lock().unwrap();
                            guard.active_synth_task.take()
                        };
                        if let Some(handle) = synth_task {
                            let _ = handle.await;
                        }

                        // Text processed; check queue for next pending items
                        continue;
                    }

                    // 2. Queue is empty. If player is still actively playing audio, wait for idle or new item.
                    if player.is_playing() {
                        tokio::select! {
                            _ = idle_rx.recv() => {
                                continue;
                            }
                            _ = notify.notified() => {
                                continue;
                            }
                        }
                    }

                    // 3. Queue is empty and player is not playing. Check atomically before exiting.
                    let mut guard = state_clone.lock().unwrap();
                    if !guard.queue.is_empty() {
                        continue;
                    }

                    // Sequence finished: emit SpeakingDone and reset status
                    if started && is_speaking.swap(false, Ordering::SeqCst) {
                        let _ = events.try_send(VoiceEvent::SpeakingDone);
                        if let Ok(cb_guard) = on_speaking_cb.lock()
                            && let Some(ref cb) = *cb_guard
                        {
                            cb(false);
                        }
                    }

                    guard.worker_task = None;
                    break;
                }
            });

            state.worker_task = Some(task);
        } else {
            self.notify.notify_one();
        }

        Ok(())
    }

    /// Instantly stops audio playback and drops queued audio (used for barge-in and StopSpeaking).
    pub fn stop(&self) {
        if let Ok(mut guard) = self.state.lock() {
            guard.queue.clear();
            if let Some(task) = guard.worker_task.take() {
                task.abort();
            }
            if let Some(task) = guard.active_synth_task.take() {
                task.abort();
            }
        }

        self.player.stop();

        if self.is_speaking.swap(false, Ordering::SeqCst) {
            let _ = self.events.try_send(VoiceEvent::SpeakingDone);
            if let Ok(guard) = self.on_speaking_cb.lock()
                && let Some(ref cb) = *guard
            {
                cb(false);
            }
        }
    }
}

impl Drop for SpeakerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Orchestrates Text-to-Speech synthesis and audio playback.
pub struct Speaker;

impl Speaker {
    /// Starts the speaker using the configured TTS command, Piper voice, and output device.
    pub fn start(
        cfg: &jgt_core::config::Voice,
        events: mpsc::Sender<VoiceEvent>,
    ) -> Result<SpeakerHandle, VoiceError> {
        let tts = match PiperCommand::new(&cfg.tts_command, &cfg.tts_voice) {
            Ok(t) => Arc::new(t),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    tts_command = %cfg.tts_command,
                    tts_voice = %cfg.tts_voice,
                    "failed to initialize TTS piper command"
                );
                return Err(e);
            }
        };
        let player = match Player::start(cfg.output_device.as_deref()) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    output_device = ?cfg.output_device,
                    "failed to start audio player"
                );
                return Err(e);
            }
        };
        Self::start_with_engine_and_player(tts, player, events)
    }

    /// Starts the speaker pipeline with explicit TTS engine and player instances.
    pub fn start_with_engine_and_player(
        tts: Arc<dyn TtsEngine>,
        player: Arc<Player>,
        events: mpsc::Sender<VoiceEvent>,
    ) -> Result<SpeakerHandle, VoiceError> {
        Ok(SpeakerHandle {
            player,
            events,
            is_speaking: Arc::new(AtomicBool::new(false)),
            on_speaking_cb: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(SpeakerState {
                queue: VecDeque::new(),
                worker_task: None,
                active_synth_task: None,
            })),
            notify: Arc::new(tokio::sync::Notify::new()),
            tts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tts::Pcm;
    use std::sync::atomic::AtomicUsize;

    #[derive(Default)]
    struct MockTts {
        call_count: Arc<AtomicUsize>,
        synthesized: Arc<Mutex<Vec<String>>>,
    }

    impl MockTts {
        fn new(call_count: Arc<AtomicUsize>) -> Self {
            Self {
                call_count,
                synthesized: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait::async_trait]
    impl TtsEngine for MockTts {
        async fn synthesize(&self, text: &str) -> Result<Pcm, VoiceError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut guard) = self.synthesized.lock() {
                guard.push(text.to_string());
            }
            // 10 samples of 48 kHz mono
            Ok(Pcm::new(vec![1000; 10], 48_000))
        }
    }

    struct GatedMockTts {
        call_count: Arc<AtomicUsize>,
        synthesized: Arc<Mutex<Vec<String>>>,
        gate: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl TtsEngine for GatedMockTts {
        async fn synthesize(&self, text: &str) -> Result<Pcm, VoiceError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut guard) = self.synthesized.lock() {
                guard.push(text.to_string());
            }
            self.gate.notified().await;
            Ok(Pcm::new(vec![1000; 10], 48_000))
        }
    }

    #[tokio::test]
    async fn test_speaker_streaming_and_callbacks() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let mock_tts = Arc::new(MockTts::new(call_count.clone()));

        let player = Arc::new(Player::headless(48_000, 1));
        let (events_tx, mut events_rx) = mpsc::channel(16);

        let speaker =
            Speaker::start_with_engine_and_player(mock_tts, player.clone(), events_tx).unwrap();

        let speaking_callback_count = Arc::new(AtomicUsize::new(0));
        let cb_count = speaking_callback_count.clone();
        speaker.set_on_speaking(move |is_speaking| {
            if is_speaking {
                cb_count.fetch_add(1, Ordering::SeqCst);
            } else {
                cb_count.fetch_add(10, Ordering::SeqCst);
            }
        });

        speaker.speak("First sentence. Second sentence!").unwrap();

        // Should receive VoiceEvent::Speaking
        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), events_rx.recv())
            .await
            .expect("timeout waiting for Speaking event")
            .unwrap();
        assert_eq!(ev, VoiceEvent::Speaking);
        assert_eq!(speaking_callback_count.load(Ordering::SeqCst), 1);

        // Wait a short moment for both sentences to be synthesized
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(call_count.load(Ordering::SeqCst), 2);

        // Drain the player audio buffer to simulate physical playback finishing
        let queue = player.queue();
        let mut buf = vec![0.0; 64];
        {
            let mut guard = queue.lock().unwrap();
            guard.read_samples(&mut buf);
        }

        // Should receive VoiceEvent::SpeakingDone
        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), events_rx.recv())
            .await
            .expect("timeout waiting for SpeakingDone event")
            .unwrap();
        assert_eq!(ev, VoiceEvent::SpeakingDone);
        assert_eq!(speaking_callback_count.load(Ordering::SeqCst), 11);
    }

    #[tokio::test]
    async fn test_speaker_barge_in_stop() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let mock_tts = Arc::new(MockTts::new(call_count.clone()));

        let player = Arc::new(Player::headless(48_000, 1));
        let (events_tx, mut events_rx) = mpsc::channel(16);

        let speaker =
            Speaker::start_with_engine_and_player(mock_tts, player.clone(), events_tx).unwrap();

        speaker.speak("Sentence one. Sentence two.").unwrap();

        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), events_rx.recv())
            .await
            .expect("timeout waiting for Speaking event")
            .unwrap();
        assert_eq!(ev, VoiceEvent::Speaking);

        // Immediate stop for barge-in
        speaker.stop();

        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), events_rx.recv())
            .await
            .expect("timeout waiting for SpeakingDone event")
            .unwrap();
        assert_eq!(ev, VoiceEvent::SpeakingDone);
        assert!(!speaker.is_speaking());
    }

    #[tokio::test]
    async fn test_speaker_queues_and_synthesizes_both_texts() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let synthesized = Arc::new(Mutex::new(Vec::new()));
        let mock_tts = Arc::new(MockTts {
            call_count: call_count.clone(),
            synthesized: synthesized.clone(),
        });

        let player = Arc::new(Player::headless(48_000, 1));
        let (events_tx, mut events_rx) = mpsc::channel(16);

        let speaker =
            Speaker::start_with_engine_and_player(mock_tts, player.clone(), events_tx).unwrap();

        let speaking_callback_count = Arc::new(AtomicUsize::new(0));
        let cb_count = speaking_callback_count.clone();
        speaker.set_on_speaking(move |is_speaking| {
            if is_speaking {
                cb_count.fetch_add(1, Ordering::SeqCst);
            } else {
                cb_count.fetch_add(10, Ordering::SeqCst);
            }
        });

        // Two Speak calls in a row
        speaker.speak("First message.").unwrap();
        speaker.speak("Second message.").unwrap();

        // First Speaking event received
        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), events_rx.recv())
            .await
            .expect("timeout waiting for Speaking event")
            .unwrap();
        assert_eq!(ev, VoiceEvent::Speaking);
        assert_eq!(speaking_callback_count.load(Ordering::SeqCst), 1);

        // Wait a short moment for both messages to be synthesized
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Both texts must have been synthesized without cutting off
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
        {
            let texts = synthesized.lock().unwrap();
            assert_eq!(texts.as_slice(), &["First message.", "Second message."]);
        }

        // Drain the player audio buffer to simulate physical playback finishing
        let queue = player.queue();
        let mut buf = vec![0.0; 64];
        {
            let mut guard = queue.lock().unwrap();
            guard.read_samples(&mut buf);
        }

        // Exactly one SpeakingDone event emitted when all speech is finished
        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), events_rx.recv())
            .await
            .expect("timeout waiting for SpeakingDone event")
            .unwrap();
        assert_eq!(ev, VoiceEvent::SpeakingDone);
        assert_eq!(speaking_callback_count.load(Ordering::SeqCst), 11);
    }

    #[tokio::test]
    async fn test_speaker_stop_clears_queue() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let synthesized = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(tokio::sync::Notify::new());
        let mock_tts = Arc::new(GatedMockTts {
            call_count: call_count.clone(),
            synthesized: synthesized.clone(),
            gate: gate.clone(),
        });

        let player = Arc::new(Player::headless(48_000, 1));
        let (events_tx, _events_rx) = mpsc::channel(16);

        let speaker =
            Speaker::start_with_engine_and_player(mock_tts, player.clone(), events_tx).unwrap();

        // 1. Speak first message (starts synthesizing, waits on gate)
        speaker.speak("First message in progress.").unwrap();

        // Wait until first message enters synthesis
        for _ in 0..100 {
            if call_count.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(call_count.load(Ordering::SeqCst), 1);

        // 2. Queue second message while first message is in progress
        speaker.speak("Second message pending in queue.").unwrap();
        assert_eq!(speaker.pending_count(), 1);

        // 3. Stop speaking: must clear pending queue, abort synthesis, and reset speaking
        speaker.stop();
        assert_eq!(speaker.pending_count(), 0);
        assert!(!speaker.is_speaking());

        // Release gate just in case
        gate.notify_one();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Second message must NEVER have been synthesized
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
        {
            let texts = synthesized.lock().unwrap();
            assert_eq!(texts.as_slice(), &["First message in progress."]);
        }
        assert_eq!(player.queue().lock().unwrap().buffered_samples(), 0);
    }

    #[tokio::test]
    async fn test_speaker_queue_bounded_drop_oldest() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let synthesized = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(tokio::sync::Notify::new());
        let mock_tts = Arc::new(GatedMockTts {
            call_count: call_count.clone(),
            synthesized: synthesized.clone(),
            gate: gate.clone(),
        });

        let player = Arc::new(Player::headless(48_000, 1));
        let (events_tx, _events_rx) = mpsc::channel(64);

        let speaker =
            Speaker::start_with_engine_and_player(mock_tts, player.clone(), events_tx).unwrap();

        // Speak initial message to occupy the worker
        speaker.speak("Initial message.").unwrap();
        for _ in 0..100 {
            if call_count.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // Queue 20 messages (exceeding MAX_PENDING_SPEECH = 16)
        for i in 1..=20 {
            speaker.speak(&format!("Pending message {i}.")).unwrap();
        }

        // Queue is bounded to 16
        assert_eq!(speaker.pending_count(), MAX_PENDING_SPEECH);

        // Clean up
        speaker.stop();
        assert_eq!(speaker.pending_count(), 0);
    }
}

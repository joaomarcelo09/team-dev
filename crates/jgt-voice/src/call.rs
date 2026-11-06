use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::error::VoiceError;
use crate::listener::{Listener, ListenerHandle};
use crate::speaker::{Speaker, SpeakerHandle};
use crate::{VoiceControl, VoiceEvent};

struct CallInner {
    listener: Mutex<Option<Arc<ListenerHandle>>>,
    speaker: Mutex<Option<Arc<SpeakerHandle>>>,
}

impl Drop for CallInner {
    fn drop(&mut self) {
        if let Ok(mut spk) = self.speaker.lock()
            && let Some(speaker) = spk.take()
        {
            speaker.stop();
        }
        if let Ok(mut list) = self.listener.lock()
            && let Some(listener) = list.take()
        {
            listener.stop();
        }
    }
}

/// Barge-in speech threshold: continuous speech duration in ms required to interrupt speaker playback (~250 ms).
pub const BARGE_IN_THRESHOLD_MS: u32 = 250;

/// Echo cancellation and mic gating mode for Coordinator voice calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EchoMode {
    /// Half-duplex: mic is paused while TTS speaks to eliminate acoustic feedback.
    #[default]
    HalfDuplex,
    /// Headset: full duplex; mic stays unmuted and active. User can interrupt Coordinator.
    Headset,
    /// PipeWire AEC: full duplex; acoustic echo cancellation handled by PipeWire echo-cancel filter.
    PipewireAec,
}

impl EchoMode {
    /// Parses an echo mode string. Unrecognized values fall back to `HalfDuplex` with a warning.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "half_duplex" | "half-duplex" => EchoMode::HalfDuplex,
            "headset" => EchoMode::Headset,
            "pipewire_aec" | "pipewire-aec" => EchoMode::PipewireAec,
            other => {
                tracing::warn!("unknown echo mode: {other:?}, falling back to half_duplex");
                EchoMode::HalfDuplex
            }
        }
    }

    /// Canonical string identifier for this echo mode.
    pub fn as_str(&self) -> &'static str {
        match self {
            EchoMode::HalfDuplex => "half_duplex",
            EchoMode::Headset => "headset",
            EchoMode::PipewireAec => "pipewire_aec",
        }
    }

    /// Returns true if this mode pauses the microphone while TTS is speaking.
    pub fn pauses_mic_during_tts(&self) -> bool {
        matches!(self, EchoMode::HalfDuplex)
    }

    /// Returns true if this mode supports barge-in interruptions.
    pub fn supports_barge_in(&self) -> bool {
        matches!(self, EchoMode::Headset | EchoMode::PipewireAec)
    }
}

/// Pure function determining whether barge-in should interrupt active speaker playback.
///
/// Returns true only when:
/// 1. Mic is unmuted (`!muted`)
/// 2. Speaker is actively playing speech (`speaking`)
/// 3. Mode supports barge-in (`mode.supports_barge_in()`)
/// 4. User speech has been continuously detected for at least `BARGE_IN_THRESHOLD_MS` (~250 ms).
pub fn should_barge_in(
    speech_ms: u32,
    speaking: bool,
    muted: bool,
    mode: EchoMode,
) -> bool {
    if muted || !speaking || !mode.supports_barge_in() {
        return false;
    }
    speech_ms >= BARGE_IN_THRESHOLD_MS
}

/// Handle to control an active voice call session.
#[derive(Clone)]
pub struct CallHandle {
    inner: Arc<CallInner>,
}

impl CallHandle {
    /// Dispatches a control command to the call session.
    pub fn send(&self, control: VoiceControl) -> Result<(), VoiceError> {
        match control {
            VoiceControl::Speak(text) => {
                if let Ok(guard) = self.inner.speaker.lock()
                    && let Some(ref speaker) = *guard
                {
                    speaker.speak(&text)?;
                }
            }
            VoiceControl::StopSpeaking => {
                if let Ok(guard) = self.inner.speaker.lock()
                    && let Some(ref speaker) = *guard
                {
                    speaker.stop();
                }
            }
            VoiceControl::Mute => {
                if let Ok(guard) = self.inner.listener.lock()
                    && let Some(ref listener) = *guard
                {
                    listener.set_muted(true);
                }
            }
            VoiceControl::Unmute => {
                if let Ok(guard) = self.inner.listener.lock()
                    && let Some(ref listener) = *guard
                {
                    listener.set_muted(false);
                }
            }
            VoiceControl::End => {
                self.stop();
            }
        }
        Ok(())
    }

    /// Stops listener, speaker and audio playback, and releases all resources.
    pub fn stop(&self) {
        if let Ok(mut spk) = self.inner.speaker.lock()
            && let Some(speaker) = spk.take()
        {
            speaker.stop();
        }
        if let Ok(mut list) = self.inner.listener.lock()
            && let Some(listener) = list.take()
        {
            listener.stop();
        }
    }

    /// Returns true if the microphone is currently muted.
    pub fn is_muted(&self) -> bool {
        self.inner
            .listener
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|l| l.is_muted()))
            .unwrap_or(false)
    }
}

/// Facade for starting and managing a Coordinator voice call session.
pub struct VoiceCall;

impl VoiceCall {
    /// Starts a full duplex voice call pipeline (listener + speaker) using the given configuration.
    ///
    /// Forwards all events (`Listening`, `Muted`, `UserSpeech`, `Speaking`, `SpeakingDone`, `Error`)
    /// to the returned event receiver.
    pub fn start(
        cfg: &jgt_core::config::Voice,
    ) -> Result<(mpsc::Receiver<VoiceEvent>, CallHandle), VoiceError> {
        let call_start_time = std::time::Instant::now();
        let (events_tx, events_rx) = mpsc::channel(64);

        let listener = match Listener::start(cfg, events_tx.clone()) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(error = %e, "failed to start listener for voice call");
                return Err(e);
            }
        };

        let speaker = match Speaker::start(cfg, events_tx) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to start speaker for voice call");
                listener.stop();
                return Err(e);
            }
        };

        let elapsed_ms = call_start_time.elapsed().as_millis() as u64;
        tracing::info!(
            input_device = %listener.device_name(),
            input_sample_rate = listener.sample_rate(),
            output_device = %speaker.player().device_name(),
            output_sample_rate = speaker.player().sample_rate(),
            elapsed_ms,
            "voice call started"
        );

        let mode = EchoMode::parse(&cfg.echo);
        Ok(Self::start_with_handles(listener, speaker, events_rx, mode))
    }

    /// Starts a voice call session with explicitly provided listener and speaker handles and echo mode.
    ///
    /// - `HalfDuplex`: while TTS is speaking, listening is paused.
    /// - `Headset` / `PipewireAec`: full duplex; user speech of ~250 ms interrupts speaker playback.
    pub fn start_with_handles(
        listener: ListenerHandle,
        speaker: SpeakerHandle,
        events_rx: mpsc::Receiver<VoiceEvent>,
        echo_mode: EchoMode,
    ) -> (mpsc::Receiver<VoiceEvent>, CallHandle) {
        let listener = Arc::new(listener);
        let listener_weak = Arc::downgrade(&listener);
        let speaker = Arc::new(speaker);
        let speaker_weak = Arc::downgrade(&speaker);

        if echo_mode.pauses_mic_during_tts() {
            // Half-duplex: while the speaker is speaking, pause listener VAD
            speaker.set_on_speaking(move |speaking| {
                if let Some(l) = listener_weak.upgrade() {
                    l.pause_for_tts(speaking);
                }
            });
        } else {
            // Full duplex (headset / pipewire_aec):
            // Never pause mic. When VAD detects ~250 ms of speech while speaking, barge in!
            let listener_for_barge = listener.clone();
            listener.set_on_speech(move |speech_ms| {
                let is_muted = listener_for_barge.is_muted();
                if let Some(s) = speaker_weak.upgrade() {
                    let is_speaking = s.is_speaking();
                    if should_barge_in(speech_ms, is_speaking, is_muted, echo_mode) {
                        s.stop();
                        listener_for_barge.send_event(VoiceEvent::Listening);
                    }
                }
            });
        }

        let handle = CallHandle {
            inner: Arc::new(CallInner {
                listener: Mutex::new(Some(listener)),
                speaker: Mutex::new(Some(speaker)),
            }),
        };

        (events_rx, handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::playback::Player;
    use crate::tts::{Pcm, TtsEngine};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct MockTts {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl TtsEngine for MockTts {
        async fn synthesize(&self, _text: &str) -> Result<Pcm, VoiceError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(Pcm::new(vec![1000; 10], 48_000))
        }
    }

    #[tokio::test]
    async fn test_voice_call_control_routing_and_half_duplex() {
        let (events_tx, events_rx) = mpsc::channel(64);
        let listener = ListenerHandle::mock(events_tx.clone());

        let tts = Arc::new(MockTts::default());
        let player = Arc::new(Player::headless(48_000, 1));
        let speaker = Speaker::start_with_engine_and_player(tts, player.clone(), events_tx).unwrap();

        let (mut events_rx, handle) =
            VoiceCall::start_with_handles(listener, speaker, events_rx, EchoMode::HalfDuplex);

        // Starts unmuted and emits Listening
        assert!(!handle.is_muted());
        let ev = events_rx.recv().await.unwrap();
        assert_eq!(ev, VoiceEvent::Listening);

        // Mute
        handle.send(VoiceControl::Mute).unwrap();
        assert!(handle.is_muted());
        let ev = events_rx.recv().await.unwrap();
        assert_eq!(ev, VoiceEvent::Muted);

        // Unmute
        handle.send(VoiceControl::Unmute).unwrap();
        assert!(!handle.is_muted());
        let ev = events_rx.recv().await.unwrap();
        assert_eq!(ev, VoiceEvent::Listening);

        // Speak triggers Speaking event and pauses listener for half-duplex
        handle.send(VoiceControl::Speak("Test sentence.".into())).unwrap();
        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), events_rx.recv())
            .await
            .expect("timeout waiting for Speaking event")
            .unwrap();
        assert_eq!(ev, VoiceEvent::Speaking);

        // Stop speaking (barge-in) triggers SpeakingDone
        handle.send(VoiceControl::StopSpeaking).unwrap();
        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), events_rx.recv())
            .await
            .expect("timeout waiting for SpeakingDone event")
            .unwrap();
        assert_eq!(ev, VoiceEvent::SpeakingDone);

        // End stops listener and speaker and closes the receiver
        handle.send(VoiceControl::End).unwrap();

        // The receiver should now be closed (returns None)
        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), events_rx.recv())
            .await
            .expect("timeout waiting for receiver close");
        assert_eq!(ev, None);
    }

    #[test]
    fn test_echo_mode_parsing() {
        assert_eq!(EchoMode::parse("half_duplex"), EchoMode::HalfDuplex);
        assert_eq!(EchoMode::parse("half-duplex"), EchoMode::HalfDuplex);
        assert_eq!(EchoMode::parse("HALF_DUPLEX"), EchoMode::HalfDuplex);
        assert_eq!(EchoMode::parse("headset"), EchoMode::Headset);
        assert_eq!(EchoMode::parse("  Headset  "), EchoMode::Headset);
        assert_eq!(EchoMode::parse("pipewire_aec"), EchoMode::PipewireAec);
        assert_eq!(EchoMode::parse("pipewire-aec"), EchoMode::PipewireAec);

        // Unknown value falls back to half_duplex
        assert_eq!(EchoMode::parse("unknown_mode"), EchoMode::HalfDuplex);
        assert_eq!(EchoMode::parse(""), EchoMode::HalfDuplex);

        assert!(EchoMode::HalfDuplex.pauses_mic_during_tts());
        assert!(!EchoMode::Headset.pauses_mic_during_tts());
        assert!(!EchoMode::PipewireAec.pauses_mic_during_tts());

        assert!(!EchoMode::HalfDuplex.supports_barge_in());
        assert!(EchoMode::Headset.supports_barge_in());
        assert!(EchoMode::PipewireAec.supports_barge_in());
    }

    #[test]
    fn test_should_barge_in_pure_threshold_logic() {
        // Below 250 ms: always false
        for ms in [0, 50, 100, 200, 249] {
            assert!(!should_barge_in(ms, true, false, EchoMode::Headset));
            assert!(!should_barge_in(ms, true, false, EchoMode::PipewireAec));
            assert!(!should_barge_in(ms, true, false, EchoMode::HalfDuplex));
        }

        // At or above 250 ms:
        // When speaking=true, muted=false, mode=Headset or PipewireAec -> true
        assert!(should_barge_in(250, true, false, EchoMode::Headset));
        assert!(should_barge_in(256, true, false, EchoMode::Headset));
        assert!(should_barge_in(500, true, false, EchoMode::Headset));
        assert!(should_barge_in(250, true, false, EchoMode::PipewireAec));
        assert!(should_barge_in(300, true, false, EchoMode::PipewireAec));

        // Half duplex never barges in
        assert!(!should_barge_in(250, true, false, EchoMode::HalfDuplex));
        assert!(!should_barge_in(500, true, false, EchoMode::HalfDuplex));

        // Not speaking -> false
        assert!(!should_barge_in(250, false, false, EchoMode::Headset));
        assert!(!should_barge_in(250, false, false, EchoMode::PipewireAec));

        // Mic muted -> false
        assert!(!should_barge_in(250, true, true, EchoMode::Headset));
        assert!(!should_barge_in(250, true, true, EchoMode::PipewireAec));

        // Both not speaking and muted -> false
        assert!(!should_barge_in(250, false, true, EchoMode::Headset));
    }

    #[tokio::test]
    async fn test_headset_no_pause_and_barge_in_interruption() {
        let (events_tx, events_rx) = mpsc::channel(64);
        let listener = ListenerHandle::mock(events_tx.clone());

        let tts = Arc::new(MockTts::default());
        let player = Arc::new(Player::headless(48_000, 1));
        let speaker =
            Speaker::start_with_engine_and_player(tts, player.clone(), events_tx).unwrap();

        let (mut events_rx, handle) =
            VoiceCall::start_with_handles(listener, speaker, events_rx, EchoMode::Headset);

        // Receive initial Listening event
        let ev = events_rx.recv().await.unwrap();
        assert_eq!(ev, VoiceEvent::Listening);

        // Start speaking
        handle.send(VoiceControl::Speak("Coordinator explaining something.".into())).unwrap();
        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev, VoiceEvent::Speaking);

        // In Headset mode, the microphone is NEVER paused during TTS (full duplex)
        assert!(
            !handle.inner.listener.lock().unwrap().as_ref().unwrap().is_paused_for_tts(),
            "microphone must not be paused in headset mode"
        );

        // Simulate 100 ms speech: below 250 ms threshold -> no barge-in
        let list_handle = handle.inner.listener.lock().unwrap().as_ref().unwrap().clone();
        list_handle.simulate_speech(100);
        assert!(handle.inner.speaker.lock().unwrap().as_ref().unwrap().is_speaking());

        // Simulate 250 ms continuous speech -> triggers barge-in!
        list_handle.simulate_speech(250);

        // Speaker stops
        assert!(
            !handle.inner.speaker.lock().unwrap().as_ref().unwrap().is_speaking(),
            "speaker should be stopped after barge-in"
        );

        // Queue is cleared
        assert_eq!(player.queue().lock().unwrap().buffered_samples(), 0);

        // SpeakingDone is emitted
        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev, VoiceEvent::SpeakingDone);

        // Listening is emitted
        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev, VoiceEvent::Listening);
    }

    #[tokio::test]
    async fn test_headset_barge_in_ignored_when_muted() {
        let (events_tx, events_rx) = mpsc::channel(64);
        let listener = ListenerHandle::mock(events_tx.clone());

        let tts = Arc::new(MockTts::default());
        let player = Arc::new(Player::headless(48_000, 1));
        let speaker =
            Speaker::start_with_engine_and_player(tts, player.clone(), events_tx).unwrap();

        let (mut events_rx, handle) =
            VoiceCall::start_with_handles(listener, speaker, events_rx, EchoMode::Headset);

        let _ = events_rx.recv().await.unwrap(); // Listening

        // Mute the microphone
        handle.send(VoiceControl::Mute).unwrap();
        let _ = events_rx.recv().await.unwrap(); // Muted

        // Start speaking
        handle.send(VoiceControl::Speak("Coordinator talking while user is muted.".into())).unwrap();
        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev, VoiceEvent::Speaking);

        // Simulate 250 ms speech while muted -> must NOT interrupt
        let list_handle = handle.inner.listener.lock().unwrap().as_ref().unwrap().clone();
        list_handle.simulate_speech(250);

        assert!(
            handle.inner.speaker.lock().unwrap().as_ref().unwrap().is_speaking(),
            "muted mic must not barge-in"
        );
    }

    #[tokio::test]
    async fn test_stop_speaking_clears_queue() {
        let (events_tx, events_rx) = mpsc::channel(64);
        let listener = ListenerHandle::mock(events_tx.clone());

        let tts = Arc::new(MockTts::default());
        let player = Arc::new(Player::headless(48_000, 1));
        let speaker =
            Speaker::start_with_engine_and_player(tts, player.clone(), events_tx).unwrap();

        let (mut events_rx, handle) =
            VoiceCall::start_with_handles(listener, speaker, events_rx, EchoMode::HalfDuplex);

        let _ = events_rx.recv().await.unwrap(); // Listening

        // Enqueue audio directly into the player
        player.enqueue(Pcm::new(vec![500; 48_000], 48_000)).unwrap();
        assert!(player.is_playing());
        assert!(player.queue().lock().unwrap().buffered_samples() > 0);

        // Send StopSpeaking
        handle.send(VoiceControl::StopSpeaking).unwrap();

        // Queue must be immediately cleared and playback stopped
        assert_eq!(player.queue().lock().unwrap().buffered_samples(), 0);
        assert!(!player.is_playing());
    }
}

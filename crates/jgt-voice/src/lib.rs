//! Voice call mode with the Coordinator for jg-team-dev.
//!
//! Planned API:
//! ```text
//! VoiceCall::start(cfg) -> (events_rx, control_tx)
//! ```
//!
//! Events:
//! - `Listening`
//! - `Hearing`
//! - `Transcribing`
//! - `NoAudio`
//! - `Muted`
//! - `UserSpeech(String)`
//! - `Speaking`
//! - `SpeakingDone`
//! - `Error`
//!
//! Controls:
//! - `Speak(String)`
//! - `StopSpeaking`
//! - `Mute`
//! - `Unmute`
//! - `End`

pub mod audio;
pub mod call;
pub mod download;
pub mod error;
pub mod listener;
pub mod models;
pub mod resample;
pub mod speaker;
pub mod speech_text;
pub mod stt;
pub mod tts;
pub mod vad;

pub use audio::playback::{AudioQueue, Player};
pub use audio::{find_device_index, get_device_name, select_device};
pub use call::{should_barge_in, CallHandle, EchoMode, VoiceCall, BARGE_IN_THRESHOLD_MS};
pub use download::{download_models, model_specs, Progress, TOTAL_MODELS_SIZE};
pub use error::VoiceError;
pub use listener::{Listener, ListenerHandle};
pub use models::{
    check_piper, check_whisper, models_dir, piper_voice_config_path, piper_voice_path,
    piper_voice_paths, whisper_model_path,
};
pub use speaker::{Speaker, SpeakerHandle};
pub use speech_text::speech_text;
pub use stt::{Stt, Transcriber};
pub use tts::{split_sentences, Pcm, PiperCommand, TtsEngine};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VoiceEvent {
    Listening,
    Hearing,
    Transcribing,
    NoAudio,
    Muted,
    UserSpeech(String),
    Speaking,
    SpeakingDone,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VoiceControl {
    Speak(String),
    StopSpeaking,
    Mute,
    Unmute,
    End,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_voice_event_and_control_variants() {
        let events = [
            VoiceEvent::Listening,
            VoiceEvent::Hearing,
            VoiceEvent::Transcribing,
            VoiceEvent::NoAudio,
            VoiceEvent::Muted,
            VoiceEvent::UserSpeech("hello coordinator".into()),
            VoiceEvent::Speaking,
            VoiceEvent::SpeakingDone,
            VoiceEvent::Error,
        ];
        for ev in &events {
            assert_eq!(ev.clone(), *ev);
        }

        let controls = [
            VoiceControl::Speak("hello user".into()),
            VoiceControl::StopSpeaking,
            VoiceControl::Mute,
            VoiceControl::Unmute,
            VoiceControl::End,
        ];
        for c in &controls {
            assert_eq!(c.clone(), *c);
        }
    }
}

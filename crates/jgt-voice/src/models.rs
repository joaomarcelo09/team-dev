use std::path::PathBuf;
use crate::error::VoiceError;

/// Supported Whisper STT models.
pub const WHISPER_DEFAULT_MODEL: &str = "medium.en-q5_0";
pub const WHISPER_SMALL_EN_Q5_1: &str = "small.en-q5_1";
pub const WHISPER_LARGE_V3_TURBO_Q5_0: &str = "large-v3-turbo-q5_0";

/// Directory where voice models and piper voices are stored (`~/.local/share/jg-team-dev/models`).
pub fn models_dir() -> PathBuf {
    jgt_core::paths::data_dir().join("models")
}

/// Resolves the file path for a Whisper STT model file (`models/whisper/ggml-<name>.bin`).
pub fn whisper_model_path(name: &str) -> PathBuf {
    let clean = name.strip_prefix("ggml-").unwrap_or(name);
    let clean = clean.strip_suffix(".bin").unwrap_or(clean);
    models_dir().join("whisper").join(format!("ggml-{clean}.bin"))
}

/// Resolves the file path for a Piper TTS voice ONNX model (`models/piper/<voice>.onnx`).
pub fn piper_voice_path(voice: &str) -> PathBuf {
    let clean = voice.strip_suffix(".onnx").unwrap_or(voice);
    models_dir().join("piper").join(format!("{clean}.onnx"))
}

/// Resolves the file path for a Piper TTS voice config JSON (`models/piper/<voice>.onnx.json`).
pub fn piper_voice_config_path(voice: &str) -> PathBuf {
    let clean = voice.strip_suffix(".onnx").unwrap_or(voice);
    models_dir().join("piper").join(format!("{clean}.onnx.json"))
}

/// Returns both paths (onnx model, json config) for a Piper voice.
pub fn piper_voice_paths(voice: &str) -> (PathBuf, PathBuf) {
    (piper_voice_path(voice), piper_voice_config_path(voice))
}

/// Checks that the Whisper STT model exists, or returns `VoiceError::MissingModel`.
pub fn check_whisper(name: &str) -> Result<PathBuf, VoiceError> {
    let path = whisper_model_path(name);
    let clean = name.strip_prefix("ggml-").unwrap_or(name);
    let clean = clean.strip_suffix(".bin").unwrap_or(clean);
    if !path.is_file() {
        return Err(VoiceError::MissingModel {
            what: format!("Whisper model '{clean}'"),
            path: path.clone(),
            hint: format!(
                "Download ggml-{clean}.bin from https://huggingface.co/ggerganov/whisper.cpp and place it at {}",
                path.display()
            ),
        });
    }
    Ok(path)
}

/// Checks that both the Piper voice ONNX model and its config JSON exist, or returns `VoiceError::MissingModel`.
pub fn check_piper(voice: &str) -> Result<PathBuf, VoiceError> {
    let onnx = piper_voice_path(voice);
    let clean = voice.strip_suffix(".onnx").unwrap_or(voice);
    if !onnx.is_file() {
        return Err(VoiceError::MissingModel {
            what: format!("Piper voice model '{clean}'"),
            path: onnx.clone(),
            hint: format!(
                "Download {clean}.onnx from https://huggingface.co/rhasspy/piper-voices and place it at {}",
                onnx.display()
            ),
        });
    }

    let json_onnx = piper_voice_config_path(voice);
    let json_alt = models_dir().join("piper").join(format!("{clean}.json"));
    if !json_onnx.is_file() && !json_alt.is_file() {
        return Err(VoiceError::MissingModel {
            what: format!("Piper voice config for '{clean}'"),
            path: json_onnx.clone(),
            hint: format!(
                "Download {clean}.onnx.json from https://huggingface.co/rhasspy/piper-voices and place it at {}",
                json_onnx.display()
            ),
        });
    }

    Ok(onnx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    static TEST_MUTEX: Mutex<()> = Mutex::new(());
    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct EnvGuard {
        prev: Option<std::ffi::OsString>,
        temp_dir: PathBuf,
    }

    impl EnvGuard {
        fn new(prefix: &str) -> Self {
            let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
            let temp_dir = std::env::temp_dir().join(format!("{prefix}-{}-{id}", std::process::id()));
            let _ = std::fs::remove_dir_all(&temp_dir);
            std::fs::create_dir_all(&temp_dir).unwrap();
            let prev = std::env::var_os("JGT_DATA_DIR");
            unsafe {
                std::env::set_var("JGT_DATA_DIR", &temp_dir);
            }
            Self { prev, temp_dir }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => unsafe { std::env::set_var("JGT_DATA_DIR", v) },
                None => unsafe { std::env::remove_var("JGT_DATA_DIR") },
            }
            let _ = std::fs::remove_dir_all(&self.temp_dir);
        }
    }

    #[test]
    fn test_path_resolvers() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let guard = EnvGuard::new("jgt-voice-paths");

        let expected_models = guard.temp_dir.join("models");
        assert_eq!(models_dir(), expected_models);

        assert_eq!(
            whisper_model_path("small.en"),
            expected_models.join("whisper/ggml-small.en.bin")
        );
        assert_eq!(
            whisper_model_path("small.en-q5_1"),
            expected_models.join("whisper/ggml-small.en-q5_1.bin")
        );
        assert_eq!(
            whisper_model_path("ggml-small.en-q5_1.bin"),
            expected_models.join("whisper/ggml-small.en-q5_1.bin")
        );
        assert_eq!(
            whisper_model_path("medium.en-q5_0"),
            expected_models.join("whisper/ggml-medium.en-q5_0.bin")
        );
        assert_eq!(
            whisper_model_path("ggml-medium.en-q5_0.bin"),
            expected_models.join("whisper/ggml-medium.en-q5_0.bin")
        );
        assert_eq!(
            whisper_model_path("large-v3-turbo-q5_0"),
            expected_models.join("whisper/ggml-large-v3-turbo-q5_0.bin")
        );
        assert_eq!(
            whisper_model_path("ggml-large-v3-turbo-q5_0.bin"),
            expected_models.join("whisper/ggml-large-v3-turbo-q5_0.bin")
        );

        assert_eq!(
            piper_voice_path("en_US-lessac-medium"),
            expected_models.join("piper/en_US-lessac-medium.onnx")
        );
        assert_eq!(
            piper_voice_config_path("en_US-lessac-medium"),
            expected_models.join("piper/en_US-lessac-medium.onnx.json")
        );
        assert_eq!(
            piper_voice_paths("en_US-lessac-medium"),
            (
                expected_models.join("piper/en_US-lessac-medium.onnx"),
                expected_models.join("piper/en_US-lessac-medium.onnx.json")
            )
        );
    }

    #[test]
    fn test_check_whisper_missing_and_found() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let _guard = EnvGuard::new("jgt-voice-whisper");

        let err = check_whisper("medium.en-q5_0").unwrap_err();
        match err {
            VoiceError::MissingModel { what, path, hint } => {
                assert!(what.contains("medium.en-q5_0"));
                assert_eq!(path, whisper_model_path("medium.en-q5_0"));
                assert!(hint.contains("https://huggingface.co/ggerganov/whisper.cpp"));
            }
            other => panic!("expected MissingModel, got {other:?}"),
        }

        // Now create the file
        let path = whisper_model_path("medium.en-q5_0");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"dummy ggml weights").unwrap();

        assert_eq!(check_whisper("medium.en-q5_0").unwrap(), path);

        // Also check backward compatibility with small.en-q5_1
        let small_path = whisper_model_path("small.en-q5_1");
        std::fs::write(&small_path, b"dummy small weights").unwrap();
        assert_eq!(check_whisper("small.en-q5_1").unwrap(), small_path);

        // Also check large-v3-turbo-q5_0
        let turbo_path = whisper_model_path("large-v3-turbo-q5_0");
        std::fs::write(&turbo_path, b"dummy turbo weights").unwrap();
        assert_eq!(check_whisper("large-v3-turbo-q5_0").unwrap(), turbo_path);
        assert_eq!(check_whisper("ggml-large-v3-turbo-q5_0.bin").unwrap(), turbo_path);
    }

    #[test]
    fn test_check_piper_missing_and_found() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let _guard = EnvGuard::new("jgt-voice-piper");

        // Missing .onnx
        let err = check_piper("en_US-lessac-medium").unwrap_err();
        match err {
            VoiceError::MissingModel { what, path, hint } => {
                assert!(what.contains("en_US-lessac-medium"));
                assert_eq!(path, piper_voice_path("en_US-lessac-medium"));
                assert!(hint.contains("https://huggingface.co/rhasspy/piper-voices"));
            }
            other => panic!("expected MissingModel, got {other:?}"),
        }

        // Create .onnx only, still missing .json
        let onnx_path = piper_voice_path("en_US-lessac-medium");
        std::fs::create_dir_all(onnx_path.parent().unwrap()).unwrap();
        std::fs::write(&onnx_path, b"dummy onnx").unwrap();

        let err = check_piper("en_US-lessac-medium").unwrap_err();
        match err {
            VoiceError::MissingModel { what, path, hint } => {
                assert!(what.contains("config"));
                assert_eq!(path, piper_voice_config_path("en_US-lessac-medium"));
                assert!(hint.contains("https://huggingface.co/rhasspy/piper-voices"));
            }
            other => panic!("expected MissingModel for config, got {other:?}"),
        }

        // Create .onnx.json
        let json_path = piper_voice_config_path("en_US-lessac-medium");
        std::fs::write(&json_path, b"{}").unwrap();

        assert_eq!(check_piper("en_US-lessac-medium").unwrap(), onnx_path);
    }

    #[test]
    fn test_large_v3_turbo_path_and_missing_hint() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let _guard = EnvGuard::new("jgt-voice-turbo");

        let err = check_whisper("large-v3-turbo-q5_0").unwrap_err();
        match err {
            VoiceError::MissingModel { what, path, hint } => {
                assert!(what.contains("large-v3-turbo-q5_0"));
                assert!(path.ends_with("whisper/ggml-large-v3-turbo-q5_0.bin"));
                assert!(hint.contains("Download ggml-large-v3-turbo-q5_0.bin from https://huggingface.co/ggerganov/whisper.cpp"));
            }
            other => panic!("expected MissingModel, got {other:?}"),
        }
    }
}

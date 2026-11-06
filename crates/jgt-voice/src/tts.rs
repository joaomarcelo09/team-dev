use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

use crate::error::VoiceError;

/// Raw 16-bit mono PCM audio buffer with sample rate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pcm {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
}

impl Pcm {
    /// Creates a new PCM buffer.
    pub fn new(samples: Vec<i16>, sample_rate: u32) -> Self {
        Self {
            samples,
            sample_rate,
        }
    }

    /// Converts raw 16-bit samples into normalized mono `f32` samples in range `[-1.0, 1.0]`.
    pub fn to_f32(&self) -> Vec<f32> {
        self.samples
            .iter()
            .map(|&s| s as f32 / 32768.0)
            .collect()
    }
}

/// Pluggable interface for Text-to-Speech synthesis engines.
#[async_trait::async_trait]
pub trait TtsEngine: Send + Sync {
    /// Synthesizes text into raw 16-bit mono PCM audio.
    async fn synthesize(&self, text: &str) -> Result<Pcm, VoiceError>;
}

/// Splits text into individual sentences for streaming TTS.
pub fn split_sentences(text: &str) -> Vec<String> {
    crate::speech_text::split_text_sentences(text)
}

/// Reads the sample rate from a Piper voice `.onnx.json` config string or defaults to 22050.
pub fn parse_piper_sample_rate(json_content: &str) -> u32 {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(json_content) else {
        return 22050;
    };

    val.get("audio")
        .and_then(|a| a.get("sample_rate"))
        .and_then(|sr| sr.as_u64())
        .map(|sr| sr as u32)
        .unwrap_or(22050)
}

/// Piper TTS engine running the external `piper` CLI executable.
pub struct PiperCommand {
    command: String,
    onnx_path: PathBuf,
    sample_rate: u32,
}

impl PiperCommand {
    /// Creates a new `PiperCommand` checking that the specified voice model exists.
    pub fn new(command: &str, voice: &str) -> Result<Self, VoiceError> {
        let onnx_path = crate::models::check_piper(voice)?;
        let config_path = crate::models::piper_voice_config_path(voice);

        let sample_rate = if config_path.is_file() {
            match std::fs::read_to_string(&config_path) {
                Ok(content) => parse_piper_sample_rate(&content),
                Err(_) => 22050,
            }
        } else {
            22050
        };

        Ok(Self {
            command: command.to_string(),
            onnx_path,
            sample_rate,
        })
    }

    /// Internal constructor allowing explicit paths (e.g. for testing).
    pub fn with_paths(command: impl Into<String>, onnx_path: PathBuf, sample_rate: u32) -> Self {
        Self {
            command: command.into(),
            onnx_path,
            sample_rate,
        }
    }

    /// Returns the detected or default sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

#[async_trait::async_trait]
impl TtsEngine for PiperCommand {
    async fn synthesize(&self, text: &str) -> Result<Pcm, VoiceError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(Pcm::new(Vec::new(), self.sample_rate));
        }

        let parts: Vec<&str> = self.command.split_whitespace().collect();
        if parts.is_empty() {
            return Err(VoiceError::Other("tts_command is empty".into()));
        }

        let program = parts[0];
        let extra_args = &parts[1..];

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(extra_args);
        cmd.arg("--model").arg(&self.onnx_path).arg("--output_raw");
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                VoiceError::CommandNotFound {
                    command: program.to_string(),
                    hint: "install piper (`pip install piper-tts` or the distro package)".to_string(),
                }
            } else {
                VoiceError::Other(format!("failed to spawn tts command '{program}': {e}"))
            }
        })?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(trimmed.as_bytes())
                .await
                .map_err(|e| VoiceError::Other(format!("failed writing to piper stdin: {e}")))?;
            stdin
                .shutdown()
                .await
                .map_err(|e| VoiceError::Other(format!("failed closing piper stdin: {e}")))?;
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| VoiceError::Other(format!("failed waiting for piper process: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(VoiceError::Other(format!(
                "piper process exited with code {:?}: {stderr}",
                output.status.code()
            )));
        }

        let samples: Vec<i16> = output
            .stdout
            .chunks_exact(2)
            .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();

        Ok(Pcm::new(samples, self.sample_rate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_sentences() {
        let input = "Hello world! How are you? I am fine. Have a great day.";
        let res = split_sentences(input);
        assert_eq!(
            res,
            vec![
                "Hello world!",
                "How are you?",
                "I am fine.",
                "Have a great day."
            ]
        );
    }

    #[test]
    fn test_split_sentences_newlines_and_extra_spaces() {
        let input = "First sentence.\n\nSecond sentence!   Third sentence?";
        let res = split_sentences(input);
        assert_eq!(
            res,
            vec!["First sentence.", "Second sentence!", "Third sentence?"]
        );
    }

    #[test]
    fn test_parse_piper_sample_rate() {
        let valid_json = r#"{
            "audio": {
                "sample_rate": 16000
            },
            "espeak": {
                "voice": "en"
            }
        }"#;
        assert_eq!(parse_piper_sample_rate(valid_json), 16000);

        let default_json = r#"{
            "other": "key"
        }"#;
        assert_eq!(parse_piper_sample_rate(default_json), 22050);

        let invalid_json = "not json at all";
        assert_eq!(parse_piper_sample_rate(invalid_json), 22050);
    }

    #[tokio::test]
    async fn test_command_not_found_error() {
        let engine = PiperCommand::with_paths(
            "nonexistent_piper_binary_12345",
            PathBuf::from("/tmp/dummy.onnx"),
            22050,
        );

        let err = engine.synthesize("Hello").await.unwrap_err();
        match err {
            VoiceError::CommandNotFound { command, hint } => {
                assert_eq!(command, "nonexistent_piper_binary_12345");
                assert!(hint.contains("pip install piper-tts"));
            }
            other => panic!("Expected CommandNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    #[ignore]
    async fn test_real_piper_synthesis() {
        let voice = "en_US-lessac-medium";
        let engine = match PiperCommand::new("piper", voice) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("Skipping real piper test: {e}");
                return;
            }
        };

        let pcm = engine.synthesize("Hello world!").await.unwrap();
        assert!(!pcm.samples.is_empty());
        assert_eq!(pcm.sample_rate, 22050);
    }
}

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use sha2::{Digest, Sha256};
use crate::error::VoiceError;
use crate::models::{piper_voice_config_path, piper_voice_path, whisper_model_path};

pub const WHISPER_MODEL_NAME: &str = "ggml-medium.en-q5_0.bin";
pub const WHISPER_MODEL_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.en-q5_0.bin";
pub const WHISPER_MODEL_SHA256: &str = "76733e26ad8fe1c7a5bf7531a9d41917b2adc0f20f2e4f5531688a8c6cd88eb0";
pub const WHISPER_MODEL_SIZE: u64 = 539_225_533;

pub const WHISPER_SMALL_EN_Q5_1_NAME: &str = "ggml-small.en-q5_1.bin";
pub const WHISPER_SMALL_EN_Q5_1_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en-q5_1.bin";
pub const WHISPER_SMALL_EN_Q5_1_SHA256: &str = "bfdff4894dcb76bbf647d56263ea2a96645423f1669176f4844a1bf8e478ad30";
pub const WHISPER_SMALL_EN_Q5_1_SIZE: u64 = 190_098_681;

pub const WHISPER_LARGE_V3_TURBO_Q5_0_NAME: &str = "ggml-large-v3-turbo-q5_0.bin";
pub const WHISPER_LARGE_V3_TURBO_Q5_0_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin";
pub const WHISPER_LARGE_V3_TURBO_Q5_0_SHA256: &str = "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2";
pub const WHISPER_LARGE_V3_TURBO_Q5_0_SIZE: u64 = 574_041_195;

pub const PIPER_VOICE_ONNX_NAME: &str = "en_US-lessac-medium.onnx";
pub const PIPER_VOICE_ONNX_URL: &str = "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/lessac/medium/en_US-lessac-medium.onnx";
pub const PIPER_VOICE_ONNX_SHA256: &str = "5efe09e69902187827af646e1a6e9d269dee769f9877d17b16b1b46eeaaf019f";
pub const PIPER_VOICE_ONNX_SIZE: u64 = 63_201_294;

pub const PIPER_VOICE_JSON_NAME: &str = "en_US-lessac-medium.onnx.json";
pub const PIPER_VOICE_JSON_URL: &str = "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/lessac/medium/en_US-lessac-medium.onnx.json";
pub const PIPER_VOICE_JSON_SHA256: &str = "efe19c417bed055f2d69908248c6ba650fa135bc868b0e6abb3da181dab690a0";
pub const PIPER_VOICE_JSON_SIZE: u64 = 4_885;

/// Total download size in bytes across default models (~602 MB; Whisper medium is ~539 MB).
pub const TOTAL_MODELS_SIZE: u64 = WHISPER_MODEL_SIZE + PIPER_VOICE_ONNX_SIZE + PIPER_VOICE_JSON_SIZE;

/// Progress information reported during model downloads.
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub file_name: String,
    pub bytes_done: u64,
    pub total_bytes: u64,
    pub overall_bytes_done: u64,
    pub overall_total_bytes: u64,
}

impl Progress {
    /// Overall download progress as a fraction from 0.0 to 1.0.
    pub fn fraction(&self) -> f32 {
        if self.overall_total_bytes == 0 {
            0.0
        } else {
            (self.overall_bytes_done as f32 / self.overall_total_bytes as f32).clamp(0.0, 1.0)
        }
    }
}

/// Specifications for a model file download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    pub name: String,
    pub url: String,
    pub expected_sha256: Option<String>,
    pub expected_size: u64,
    pub dest_path: PathBuf,
}

/// Computes the hex-encoded SHA-256 hash of a file.
pub fn compute_sha256(path: &Path) -> std::io::Result<String> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Verifies that `path` has the expected SHA-256 hash.
///
/// If the hash does not match, the file is immediately removed and an error is returned.
pub fn verify_file_sha256(path: &Path, expected_sha256: &str) -> Result<bool, VoiceError> {
    let hash = compute_sha256(path)
        .map_err(|e| VoiceError::Other(format!("failed reading {}: {e}", path.display())))?;
    if hash.eq_ignore_ascii_case(expected_sha256) {
        Ok(true)
    } else {
        let _ = std::fs::remove_file(path);
        Err(VoiceError::Other(format!(
            "SHA-256 mismatch for {}: expected {}, got {}. Corrupted file removed.",
            path.display(),
            expected_sha256,
            hash
        )))
    }
}

/// Returns the model download specification for a given Whisper model name.
pub fn whisper_spec(model_name: &str) -> ModelSpec {
    let clean = model_name.strip_prefix("ggml-").unwrap_or(model_name);
    let clean = clean.strip_suffix(".bin").unwrap_or(clean);
    let dest_path = whisper_model_path(model_name);

    if clean == "medium.en-q5_0" {
        ModelSpec {
            name: WHISPER_MODEL_NAME.to_string(),
            url: WHISPER_MODEL_URL.to_string(),
            expected_sha256: Some(WHISPER_MODEL_SHA256.to_string()),
            expected_size: WHISPER_MODEL_SIZE,
            dest_path,
        }
    } else if clean == "small.en-q5_1" {
        ModelSpec {
            name: WHISPER_SMALL_EN_Q5_1_NAME.to_string(),
            url: WHISPER_SMALL_EN_Q5_1_URL.to_string(),
            expected_sha256: Some(WHISPER_SMALL_EN_Q5_1_SHA256.to_string()),
            expected_size: WHISPER_SMALL_EN_Q5_1_SIZE,
            dest_path,
        }
    } else if clean == "large-v3-turbo-q5_0" {
        ModelSpec {
            name: WHISPER_LARGE_V3_TURBO_Q5_0_NAME.to_string(),
            url: WHISPER_LARGE_V3_TURBO_Q5_0_URL.to_string(),
            expected_sha256: Some(WHISPER_LARGE_V3_TURBO_Q5_0_SHA256.to_string()),
            expected_size: WHISPER_LARGE_V3_TURBO_Q5_0_SIZE,
            dest_path,
        }
    } else {
        let filename = format!("ggml-{clean}.bin");
        ModelSpec {
            name: filename.clone(),
            url: format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{filename}"),
            expected_sha256: None,
            expected_size: 0,
            dest_path,
        }
    }
}

/// Returns the model download specifications based on voice configuration.
pub fn model_specs(cfg: &jgt_core::config::Voice) -> Vec<ModelSpec> {
    vec![
        whisper_spec(&cfg.stt_model),
        ModelSpec {
            name: PIPER_VOICE_ONNX_NAME.to_string(),
            url: PIPER_VOICE_ONNX_URL.to_string(),
            expected_sha256: Some(PIPER_VOICE_ONNX_SHA256.to_string()),
            expected_size: PIPER_VOICE_ONNX_SIZE,
            dest_path: piper_voice_path(&cfg.tts_voice),
        },
        ModelSpec {
            name: PIPER_VOICE_JSON_NAME.to_string(),
            url: PIPER_VOICE_JSON_URL.to_string(),
            expected_sha256: Some(PIPER_VOICE_JSON_SHA256.to_string()),
            expected_size: PIPER_VOICE_JSON_SIZE,
            dest_path: piper_voice_config_path(&cfg.tts_voice),
        },
    ]
}

/// Downloads Whisper and Piper voice models via curl, verifying pinned SHA-256 hashes.
///
/// - Writes to `<file>.part` before verifying and renaming.
/// - Polls file size during download to report progress.
/// - Skips files already present with a matching SHA-256.
/// - On hash mismatch, deletes the corrupted file and returns an error.
/// - Never runs automatically: should only be invoked after explicit user confirmation.
pub async fn download_models<F>(
    cfg: &jgt_core::config::Voice,
    progress: F,
) -> Result<(), VoiceError>
where
    F: Fn(Progress) + Send + Sync,
{
    let specs = model_specs(cfg);
    let total_all_bytes: u64 = specs.iter().map(|s| s.expected_size).sum();
    let mut completed_bytes: u64 = 0;

    for spec in specs {
        // Ensure destination parent directory exists
        if let Some(parent) = spec.dest_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                VoiceError::Other(format!(
                    "failed to create directory {}: {e}",
                    parent.display()
                ))
            })?;
        }

        // Check if destination file already exists
        if spec.dest_path.is_file() {
            if let Some(expected_sha256) = &spec.expected_sha256 {
                match verify_file_sha256(&spec.dest_path, expected_sha256) {
                    Ok(true) => {
                        tracing::info!("{}: already downloaded with matching SHA-256, skipping", spec.name);
                        completed_bytes += spec.expected_size;
                        progress(Progress {
                            file_name: spec.name.clone(),
                            bytes_done: spec.expected_size,
                            total_bytes: spec.expected_size,
                            overall_bytes_done: completed_bytes,
                            overall_total_bytes: total_all_bytes,
                        });
                        continue;
                    }
                    Err(err) => {
                        // Existing file was corrupted and has been deleted; return error
                        return Err(err);
                    }
                    Ok(false) => unreachable!(),
                }
            } else {
                let file_len = std::fs::metadata(&spec.dest_path).map(|m| m.len()).unwrap_or(0);
                if file_len > 0 {
                    tracing::info!("{}: already downloaded, skipping", spec.name);
                    completed_bytes += spec.expected_size;
                    progress(Progress {
                        file_name: spec.name.clone(),
                        bytes_done: file_len,
                        total_bytes: file_len,
                        overall_bytes_done: completed_bytes,
                        overall_total_bytes: total_all_bytes,
                    });
                    continue;
                }
            }
        }

        let part_path = spec.dest_path.with_extension(format!(
            "{}.part",
            spec.dest_path.extension().and_then(|e| e.to_str()).unwrap_or("bin")
        ));

        // Remove any stale .part file from an interrupted download
        if part_path.exists() {
            let _ = std::fs::remove_file(&part_path);
        }

        progress(Progress {
            file_name: spec.name.clone(),
            bytes_done: 0,
            total_bytes: spec.expected_size,
            overall_bytes_done: completed_bytes,
            overall_total_bytes: total_all_bytes,
        });

        // Spawn curl -L --fail -o <part_path> <url>
        let mut child = tokio::process::Command::new("curl")
            .arg("-L")
            .arg("--fail")
            .arg("-o")
            .arg(&part_path)
            .arg(&spec.url)
            .spawn()
            .map_err(|e| {
                VoiceError::Other(format!(
                    "failed to spawn curl for downloading {}: {e}",
                    spec.name
                ))
            })?;

        // Poll file size while child is running
        loop {
            tokio::select! {
                status_res = child.wait() => {
                    let status = status_res.map_err(|e| VoiceError::Other(format!("failed waiting for curl: {e}")))?;
                    if !status.success() {
                        let _ = std::fs::remove_file(&part_path);
                        return Err(VoiceError::Other(format!(
                            "curl download failed for {} with exit status {:?}",
                            spec.name,
                            status.code()
                        )));
                    }
                    break;
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(150)) => {
                    let current_bytes = std::fs::metadata(&part_path).map(|m| m.len()).unwrap_or(0);
                    let overall_total = total_all_bytes.max(completed_bytes + current_bytes);
                    progress(Progress {
                        file_name: spec.name.clone(),
                        bytes_done: current_bytes,
                        total_bytes: spec.expected_size,
                        overall_bytes_done: (completed_bytes + current_bytes).min(overall_total),
                        overall_total_bytes: overall_total,
                    });
                }
            }
        }

        // Verify SHA-256 of the downloaded .part file if known
        if let Some(expected_sha256) = &spec.expected_sha256 {
            verify_file_sha256(&part_path, expected_sha256)?;
        }

        // Rename .part to final destination path
        std::fs::rename(&part_path, &spec.dest_path).map_err(|e| {
            VoiceError::Other(format!(
                "failed to rename {} to {}: {e}",
                part_path.display(),
                spec.dest_path.display()
            ))
        })?;

        completed_bytes += spec.expected_size;
        let overall_total = total_all_bytes.max(completed_bytes);
        progress(Progress {
            file_name: spec.name.clone(),
            bytes_done: spec.expected_size,
            total_bytes: spec.expected_size,
            overall_bytes_done: completed_bytes,
            overall_total_bytes: overall_total,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    static TEST_MUTEX: Mutex<()> = Mutex::new(());
    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDirGuard {
        dir: PathBuf,
    }

    impl TempDirGuard {
        fn new(prefix: &str) -> Self {
            let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("{prefix}-{}-{id}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn test_compute_sha256_known_string() {
        let guard = TempDirGuard::new("jgt-test-sha256");
        let file = guard.dir.join("test.txt");
        std::fs::write(&file, b"hello world\n").unwrap();
        // SHA-256 of "hello world\n" is a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447
        let hash = compute_sha256(&file).unwrap();
        assert_eq!(hash, "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447");
    }

    #[test]
    fn test_verify_file_sha256_valid_keeps_file() {
        let guard = TempDirGuard::new("jgt-test-verify-valid");
        let file = guard.dir.join("valid.bin");
        std::fs::write(&file, b"hello world\n").unwrap();

        let res = verify_file_sha256(&file, "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447");
        assert!(res.unwrap());
        assert!(file.exists(), "valid file must remain intact");
    }

    #[test]
    fn test_verify_file_sha256_mismatch_deletes_file_and_errors() {
        let guard = TempDirGuard::new("jgt-test-verify-mismatch");
        let file = guard.dir.join("corrupted.bin");
        std::fs::write(&file, b"corrupted bytes").unwrap();

        let wrong_hash = "0000000000000000000000000000000000000000000000000000000000000000";
        let res = verify_file_sha256(&file, wrong_hash);
        assert!(res.is_err());
        assert!(!file.exists(), "corrupted file must be deleted");
        let err_msg = res.unwrap_err().to_string();
        assert!(err_msg.contains("SHA-256 mismatch"));
        assert!(err_msg.contains("Corrupted file removed"));
    }

    #[test]
    fn test_skip_already_present_with_matching_hash() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let guard = TempDirGuard::new("jgt-test-skip");
        let test_data = b"whisper model test content";
        let mut hasher = Sha256::new();
        hasher.update(test_data);
        let expected_hash = format!("{:x}", hasher.finalize());

        let target_file = guard.dir.join("target.bin");
        std::fs::write(&target_file, test_data).unwrap();

        // Verify that checking the file confirms matching hash
        assert!(target_file.is_file());
        let verified = verify_file_sha256(&target_file, &expected_hash);
        assert!(verified.unwrap());
        assert!(target_file.is_file());
    }

    #[test]
    fn test_part_file_verification_and_rename_flow() {
        let guard = TempDirGuard::new("jgt-test-part-flow");
        let final_path = guard.dir.join("model.bin");
        let part_path = guard.dir.join("model.bin.part");

        let content = b"fake weights 123456789";
        let mut hasher = Sha256::new();
        hasher.update(content);
        let valid_hash = format!("{:x}", hasher.finalize());

        std::fs::write(&part_path, content).unwrap();
        assert!(part_path.exists());
        assert!(!final_path.exists());

        // Verify part file and rename
        verify_file_sha256(&part_path, &valid_hash).unwrap();
        std::fs::rename(&part_path, &final_path).unwrap();

        assert!(!part_path.exists());
        assert!(final_path.exists());
        assert_eq!(std::fs::read(&final_path).unwrap(), content);
    }

    #[test]
    fn test_progress_fraction_calculation() {
        let prog = Progress {
            file_name: "test.bin".into(),
            bytes_done: 50,
            total_bytes: 100,
            overall_bytes_done: 50,
            overall_total_bytes: 200,
        };
        assert!((prog.fraction() - 0.25).abs() < f32::EPSILON);
    }

    #[test]
    fn test_model_specs_default_and_backward_compat() {
        let default_cfg = jgt_core::config::Voice::default();
        let specs = model_specs(&default_cfg);
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].name, WHISPER_MODEL_NAME);
        assert_eq!(specs[0].url, WHISPER_MODEL_URL);
        assert_eq!(specs[0].expected_sha256.as_deref(), Some(WHISPER_MODEL_SHA256));
        assert_eq!(specs[0].expected_size, WHISPER_MODEL_SIZE);

        let small_cfg = jgt_core::config::Voice {
            stt_model: "small.en-q5_1".into(),
            ..Default::default()
        };
        let small_specs = model_specs(&small_cfg);
        assert_eq!(small_specs[0].name, WHISPER_SMALL_EN_Q5_1_NAME);
        assert_eq!(small_specs[0].url, WHISPER_SMALL_EN_Q5_1_URL);
        assert_eq!(small_specs[0].expected_sha256.as_deref(), Some(WHISPER_SMALL_EN_Q5_1_SHA256));
        assert_eq!(small_specs[0].expected_size, WHISPER_SMALL_EN_Q5_1_SIZE);

        let custom_cfg = jgt_core::config::Voice {
            stt_model: "base.en".into(),
            ..Default::default()
        };
        let custom_specs = model_specs(&custom_cfg);
        assert_eq!(custom_specs[0].name, "ggml-base.en.bin");
        assert_eq!(
            custom_specs[0].url,
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin"
        );
        assert_eq!(custom_specs[0].expected_sha256, None);

        let turbo_cfg = jgt_core::config::Voice {
            stt_model: "large-v3-turbo-q5_0".into(),
            ..Default::default()
        };
        let turbo_specs = model_specs(&turbo_cfg);
        assert_eq!(turbo_specs[0].name, WHISPER_LARGE_V3_TURBO_Q5_0_NAME);
        assert_eq!(turbo_specs[0].url, WHISPER_LARGE_V3_TURBO_Q5_0_URL);
        assert_eq!(
            turbo_specs[0].expected_sha256.as_deref(),
            Some(WHISPER_LARGE_V3_TURBO_Q5_0_SHA256)
        );
        assert_eq!(turbo_specs[0].expected_size, WHISPER_LARGE_V3_TURBO_Q5_0_SIZE);

        // Also verify clean handling when specified with full filename
        let spec_from_filename = whisper_spec("ggml-large-v3-turbo-q5_0.bin");
        assert_eq!(spec_from_filename.name, WHISPER_LARGE_V3_TURBO_Q5_0_NAME);
        assert_eq!(spec_from_filename.url, WHISPER_LARGE_V3_TURBO_Q5_0_URL);
        assert_eq!(
            spec_from_filename.expected_sha256.as_deref(),
            Some(WHISPER_LARGE_V3_TURBO_Q5_0_SHA256)
        );
        assert_eq!(spec_from_filename.expected_size, WHISPER_LARGE_V3_TURBO_Q5_0_SIZE);
    }

    #[test]
    fn test_whisper_spec_large_v3_turbo_q5_0() {
        let spec = whisper_spec("large-v3-turbo-q5_0");
        assert_eq!(spec.name, "ggml-large-v3-turbo-q5_0.bin");
        assert_eq!(
            spec.url,
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin"
        );
        assert_eq!(
            spec.expected_sha256.as_deref(),
            Some("394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2")
        );
        assert_eq!(spec.expected_size, 574_041_195);
        assert!(spec.dest_path.ends_with("whisper/ggml-large-v3-turbo-q5_0.bin"));
    }

    #[tokio::test]
    #[ignore = "downloads ggml-large-v3-turbo-q5_0.bin (~547 MiB)"]
    async fn test_download_large_v3_turbo_q5_0() {
        let cfg = jgt_core::config::Voice {
            stt_model: "large-v3-turbo-q5_0".into(),
            ..Default::default()
        };
        download_models(&cfg, |prog| {
            if prog.bytes_done == prog.total_bytes || prog.bytes_done % (50 * 1024 * 1024) < 100_000 {
                println!(
                    "Progress: {} -> {:.1}% ({}/{} MB)",
                    prog.file_name,
                    prog.fraction() * 100.0,
                    prog.bytes_done / (1024 * 1024),
                    prog.total_bytes / (1024 * 1024)
                );
            }
        })
        .await
        .expect("download_models failed");

        let dest = crate::models::whisper_model_path("large-v3-turbo-q5_0");
        assert!(dest.is_file(), "model must exist at {}", dest.display());
        let valid = verify_file_sha256(&dest, WHISPER_LARGE_V3_TURBO_Q5_0_SHA256)
            .expect("SHA-256 verify failed");
        assert!(valid, "SHA-256 hash check failed");
        println!(
            "Successfully downloaded and verified SHA-256 for {}",
            dest.display()
        );
    }
}

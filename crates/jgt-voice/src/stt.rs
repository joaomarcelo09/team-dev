use std::path::Path;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::error::VoiceError;

/// Domain vocabulary initial prompt to bias Whisper towards project terminology.
pub const INITIAL_PROMPT: &str =
    "Coordinator, task, branch, merge, worktree, reviewer, verifier, planner, backlog, pkgrel, Rust, iced, JGT, large-v3-turbo.";

/// Whisper limits the initial prompt context to ~224 tokens.
pub const MAX_INITIAL_PROMPT_TOKENS: usize = 224;

/// Approximate character length limit for ~224 tokens (~4 characters per token).
pub const MAX_INITIAL_PROMPT_CHARS: usize = MAX_INITIAL_PROMPT_TOKENS * 4;

/// Returns `INITIAL_PROMPT` capped to Whisper's prompt limit.
pub fn capped_initial_prompt() -> &'static str {
    cap_prompt(INITIAL_PROMPT)
}

/// Caps a prompt string so that it stays within Whisper's prompt limit (~224 tokens / ~896 chars).
pub fn cap_prompt(prompt: &str) -> &str {
    if prompt.len() <= MAX_INITIAL_PROMPT_CHARS {
        prompt
    } else {
        &prompt[..prompt.floor_char_boundary(MAX_INITIAL_PROMPT_CHARS)]
    }
}

/// Normalizes a transcript by converting to lowercase, removing punctuation, and collapsing whitespace.
pub fn normalize_transcript(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else {
            out.push(' ');
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Returns true if `text` echoes Whisper's `INITIAL_PROMPT`.
///
/// A transcript is considered a prompt echo if its normalized form (lowercase, punctuation
/// removed, whitespace collapsed):
/// 1. equals the prompt,
/// 2. is contained in it as a multi-word sequence (3+ words), or
/// 3. starts with a run of 4+ consecutive prompt words in prompt order.
///
/// Single words and short ordinary speech from domain vocabulary are NOT considered prompt echoes.
pub fn is_prompt_echo(text: &str) -> bool {
    is_prompt_echo_against(text, capped_initial_prompt())
}

/// Checks whether `text` echoes `prompt` based on normalization, multi-word phrase containment,
/// and consecutive word prefix matching.
pub fn is_prompt_echo_against(text: &str, prompt: &str) -> bool {
    let norm_text = normalize_transcript(text);
    if norm_text.is_empty() {
        return false;
    }

    let norm_prompt = normalize_transcript(prompt);
    if norm_prompt.is_empty() {
        return false;
    }

    // 1. Equals the prompt
    if norm_text == norm_prompt {
        return true;
    }

    let prompt_words: Vec<&str> = norm_prompt.split_whitespace().collect();
    let text_words: Vec<&str> = norm_text.split_whitespace().collect();

    // 2. Is contained in it as a multi-word sequence (at least 3 words)
    if text_words.len() >= 3 {
        let padded_prompt = format!(" {norm_prompt} ");
        let padded_text = format!(" {norm_text} ");
        if padded_prompt.contains(&padded_text) {
            return true;
        }
    }

    // 3. Starts with a run of 4+ consecutive prompt words in prompt order
    if text_words.len() >= 4 {
        let first_4 = &text_words[0..4];
        if prompt_words.windows(4).any(|w| w == first_4) {
            return true;
        }
    }

    false
}

/// Strips blank-audio markers, bracketed sound markers, and whitespace from transcribed text.
/// If the text contains no alphanumeric characters, an empty string is returned.
/// If the resulting transcript echoes the initial prompt, it is dropped and an empty string is returned.
pub fn clean_transcript(raw: &str) -> String {
    let mut cleaned = raw.to_string();
    const MARKERS: &[&str] = &[
        "(blank audio)",
        "(music)",
        "(sound)",
        "(noise)",
        "(laughter)",
        "(applause)",
    ];

    for marker in MARKERS {
        cleaned = cleaned.replace(marker, "");
    }

    // Strip any bracketed annotations like [BLANK_AUDIO], [MUSIC PLAYING], [END], [NOISE], etc.
    let mut no_brackets = String::with_capacity(cleaned.len());
    let mut depth = 0usize;
    for c in cleaned.chars() {
        match c {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            _ => {
                if depth == 0 {
                    no_brackets.push(c);
                }
            }
        }
    }

    let cleaned = no_brackets.trim();
    if cleaned.is_empty() || !cleaned.chars().any(|c| c.is_alphanumeric()) {
        return String::new();
    }

    if is_prompt_echo(cleaned) {
        tracing::debug!(cleaned, "dropping transcript echoing initial prompt");
        return String::new();
    }

    cleaned.to_string()
}

/// Trait for transcribing 16 kHz mono `f32` PCM audio buffers to text.
pub trait Transcriber: Send + Sync + 'static {
    fn transcribe(&self, audio: &[f32]) -> Result<String, VoiceError>;
}

/// Speech-to-text engine using Whisper.
pub struct Stt {
    ctx: WhisperContext,
    threads: Option<u16>,
}

impl Transcriber for Stt {
    fn transcribe(&self, audio: &[f32]) -> Result<String, VoiceError> {
        self.transcribe(audio)
    }
}

impl Stt {
    /// Loads a Whisper model from `path` with the specified decoding threads.
    pub fn load(path: impl AsRef<Path>, threads: impl Into<Option<u16>>) -> Result<Self, VoiceError> {
        let path = path.as_ref();
        if !path.is_file() {
            return Err(VoiceError::Other(format!(
                "Whisper model file not found: {}",
                path.display()
            )));
        }

        whisper_rs::install_logging_hooks();

        let ctx_params = WhisperContextParameters::default();
        let ctx = WhisperContext::new_with_params(path, ctx_params)
            .map_err(|e| VoiceError::Other(format!("failed to initialize whisper context: {e:?}")))?;

        Ok(Self {
            ctx,
            threads: threads.into(),
        })
    }

    /// Transcribes a 16 kHz mono `f32` PCM audio buffer to English text.
    ///
    /// Beam search decoding is used without timestamps. Blank-audio markers and surrounding whitespace
    /// are stripped. Transcripts echoing the initial prompt are dropped.
    pub fn transcribe(&self, audio: &[f32]) -> Result<String, VoiceError> {
        let (raw, _) = self.transcribe_raw_with_prompt(audio, Some(capped_initial_prompt()))?;
        Ok(clean_transcript(&raw))
    }

    /// Internal method to transcribe with an optional prompt and return the raw output and segment count.
    pub fn transcribe_raw_with_prompt(
        &self,
        audio: &[f32],
        prompt: Option<&str>,
    ) -> Result<(String, usize), VoiceError> {
        if audio.is_empty() || audio.iter().all(|&s| s == 0.0) {
            return Ok((String::new(), 0));
        }

        let mut state = self
            .ctx
            .create_state()
            .map_err(|e| VoiceError::Other(format!("failed to create whisper state: {e:?}")))?;

        let mut params = FullParams::new(SamplingStrategy::BeamSearch {
            beam_size: 5,
            patience: -1.0,
        });
        params.set_language(Some("en"));
        params.set_no_timestamps(true);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_no_context(true);
        params.set_suppress_blank(true);
        params.set_temperature(0.0);
        if let Some(p) = prompt {
            params.set_initial_prompt(p);
        }

        let thread_count = self.threads.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| (n.get() as u16).clamp(1, 8))
                .unwrap_or(4)
        }) as i32;
        params.set_n_threads(thread_count);

        let t0 = std::time::Instant::now();
        state
            .full(params, audio)
            .map_err(|e| VoiceError::Other(format!("whisper transcription failed: {e:?}")))?;
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        tracing::debug!(elapsed_ms, audio_samples = audio.len(), "whisper state.full completed");

        let num_segments = state.full_n_segments();
        let mut raw_text = String::new();
        for i in 0..num_segments {
            if let Some(segment) = state.get_segment(i)
                && let Ok(text) = segment.to_str()
            {
                raw_text.push_str(text);
            }
        }

        Ok((raw_text, num_segments as usize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_transcript() {
        assert_eq!(clean_transcript("  [BLANK_AUDIO]  "), "");
        assert_eq!(
            clean_transcript("Hello world [BLANK_AUDIO]"),
            "Hello world"
        );
        assert_eq!(
            clean_transcript("[blank audio] start [NOISE] middle [MUSIC] end"),
            "start  middle  end"
        );
        assert_eq!(clean_transcript("   already clean text   "), "already clean text");
        assert_eq!(clean_transcript(">>"), "");
        assert_eq!(clean_transcript("..."), "");
        assert_eq!(clean_transcript("-"), "");
        assert_eq!(clean_transcript(">>  ... --"), "");
        assert_eq!(clean_transcript("   !@#$%^&*()_+   "), "");
        assert_eq!(clean_transcript("[MUSIC PLAYING]"), "");
        assert_eq!(clean_transcript("[END]"), "");
        assert_eq!(clean_transcript("(blank audio)"), "");
    }

    #[test]
    fn test_prompt_echo_full_prompt_dropped() {
        assert_eq!(clean_transcript(INITIAL_PROMPT), "");
        assert_eq!(clean_transcript(capped_initial_prompt()), "");
        assert!(is_prompt_echo(INITIAL_PROMPT));
    }

    #[test]
    fn test_prompt_echo_prefix_and_subruns_dropped() {
        // Run of 3+ consecutive prompt words in prompt order
        assert_eq!(clean_transcript("Coordinator, task, branch"), "");
        assert!(is_prompt_echo("Coordinator, task, branch"));

        assert_eq!(clean_transcript("task, branch, merge"), "");
        assert!(is_prompt_echo("task, branch, merge"));

        assert_eq!(clean_transcript("reviewer, verifier, planner"), "");
        assert!(is_prompt_echo("reviewer, verifier, planner"));

        assert_eq!(clean_transcript("Rust, iced, JGT"), "");
        assert!(is_prompt_echo("Rust, iced, JGT"));

        // Prefix/sub-run with bracket markers stripped before checking echo
        assert_eq!(
            clean_transcript("[BLANK_AUDIO] Coordinator, task, branch [NOISE]"),
            ""
        );
    }

    #[test]
    fn test_prompt_echo_starts_with_consecutive_words_dropped() {
        // Starts with 4 consecutive prompt words, then trailing non-prompt speech
        assert_eq!(
            clean_transcript("Coordinator, task, branch, merge, how is the task going?"),
            ""
        );
        assert!(is_prompt_echo(
            "Coordinator, task, branch, merge, how is the task going?"
        ));

        assert_eq!(
            clean_transcript("reviewer, verifier, planner, backlog, check the issues"),
            ""
        );
        assert!(is_prompt_echo(
            "reviewer, verifier, planner, backlog, check the issues"
        ));
    }

    #[test]
    fn test_prompt_echo_single_words_and_short_phrases_kept() {
        // Single words from prompt must NOT be dropped
        assert!(!is_prompt_echo("task"));
        assert_eq!(clean_transcript("task"), "task");

        assert!(!is_prompt_echo("merge"));
        assert_eq!(clean_transcript("merge"), "merge");

        assert!(!is_prompt_echo("branch"));
        assert_eq!(clean_transcript("branch"), "branch");

        assert!(!is_prompt_echo("coordinator"));
        assert_eq!(clean_transcript("coordinator"), "coordinator");

        assert!(!is_prompt_echo("worktree"));
        assert_eq!(clean_transcript("worktree"), "worktree");

        assert!(!is_prompt_echo("reviewer"));
        assert_eq!(clean_transcript("reviewer"), "reviewer");

        assert!(!is_prompt_echo("verifier"));
        assert_eq!(clean_transcript("verifier"), "verifier");

        assert!(!is_prompt_echo("planner"));
        assert_eq!(clean_transcript("planner"), "planner");

        assert!(!is_prompt_echo("backlog"));
        assert_eq!(clean_transcript("backlog"), "backlog");

        assert!(!is_prompt_echo("pkgrel"));
        assert_eq!(clean_transcript("pkgrel"), "pkgrel");

        assert!(!is_prompt_echo("Rust"));
        assert_eq!(clean_transcript("Rust"), "Rust");

        assert!(!is_prompt_echo("iced"));
        assert_eq!(clean_transcript("iced"), "iced");

        assert!(!is_prompt_echo("JGT"));
        assert_eq!(clean_transcript("JGT"), "JGT");

        // Two words
        assert!(!is_prompt_echo("merge branch"));
        assert_eq!(clean_transcript("merge branch"), "merge branch");

        assert!(!is_prompt_echo("Rust iced"));
        assert_eq!(clean_transcript("Rust iced"), "Rust iced");
    }

    #[test]
    fn test_prompt_echo_ordinary_speech_using_prompt_words_kept() {
        // Real speech using prompt words must pass
        assert_eq!(
            clean_transcript("ask the coordinator to merge the branch"),
            "ask the coordinator to merge the branch"
        );
        assert!(!is_prompt_echo("ask the coordinator to merge the branch"));

        assert_eq!(
            clean_transcript("what is the status of the task"),
            "what is the status of the task"
        );
        assert!(!is_prompt_echo("what is the status of the task"));

        assert_eq!(
            clean_transcript("Coordinator, what should we work on next?"),
            "Coordinator, what should we work on next?"
        );
        assert!(!is_prompt_echo("Coordinator, what should we work on next?"));

        assert_eq!(
            clean_transcript("I will create a new branch and test it"),
            "I will create a new branch and test it"
        );
        assert!(!is_prompt_echo("I will create a new branch and test it"));

        assert_eq!(
            clean_transcript("switch to the worktree and run cargo test"),
            "switch to the worktree and run cargo test"
        );
        assert!(!is_prompt_echo("switch to the worktree and run cargo test"));

        assert_eq!(
            clean_transcript("the reviewer and verifier passed"),
            "the reviewer and verifier passed"
        );
        assert!(!is_prompt_echo("the reviewer and verifier passed"));

        assert_eq!(
            clean_transcript("check the backlog in JGT"),
            "check the backlog in JGT"
        );
        assert!(!is_prompt_echo("check the backlog in JGT"));

        assert_eq!(
            clean_transcript("bump pkgrel for the new release"),
            "bump pkgrel for the new release"
        );
        assert!(!is_prompt_echo("bump pkgrel for the new release"));

        assert_eq!(
            clean_transcript("the app is written in Rust using iced"),
            "the app is written in Rust using iced"
        );
        assert!(!is_prompt_echo("the app is written in Rust using iced"));

        assert_eq!(
            clean_transcript("transcription with large-v3-turbo is faster"),
            "transcription with large-v3-turbo is faster"
        );
        assert!(!is_prompt_echo(
            "transcription with large-v3-turbo is faster"
        ));
    }

    #[test]
    fn test_normalize_transcript() {
        assert_eq!(
            normalize_transcript("  Coordinator,   Board, task!  "),
            "coordinator board task"
        );
        assert_eq!(
            normalize_transcript("jg-team-dev"),
            "jg team dev"
        );
        assert_eq!(
            normalize_transcript(""),
            ""
        );
        assert_eq!(
            normalize_transcript("   "),
            ""
        );
    }

    #[test]
    fn test_initial_prompt_constant() {
        assert!(!INITIAL_PROMPT.is_empty(), "initial prompt must not be empty");
        // Verify key domain terms from the prompt are present
        assert!(INITIAL_PROMPT.contains("Coordinator"));
        assert!(INITIAL_PROMPT.contains("task"));
        assert!(INITIAL_PROMPT.contains("branch"));
        assert!(INITIAL_PROMPT.contains("merge"));
        assert!(INITIAL_PROMPT.contains("worktree"));
        assert!(INITIAL_PROMPT.contains("reviewer"));
        assert!(INITIAL_PROMPT.contains("verifier"));
        assert!(INITIAL_PROMPT.contains("planner"));
        assert!(INITIAL_PROMPT.contains("backlog"));
        assert!(INITIAL_PROMPT.contains("pkgrel"));
        assert!(INITIAL_PROMPT.contains("Rust"));
        assert!(INITIAL_PROMPT.contains("iced"));
        assert!(INITIAL_PROMPT.contains("JGT"));
        assert!(INITIAL_PROMPT.contains("large-v3-turbo"));

        // Whisper initial prompt limit is ~224 tokens.
        let word_count = INITIAL_PROMPT.split_whitespace().count();
        assert!(
            word_count <= MAX_INITIAL_PROMPT_TOKENS,
            "prompt word count ({word_count}) exceeds limit ({MAX_INITIAL_PROMPT_TOKENS})"
        );
        assert!(
            INITIAL_PROMPT.len() <= MAX_INITIAL_PROMPT_CHARS,
            "prompt character length ({}) exceeds limit ({MAX_INITIAL_PROMPT_CHARS})",
            INITIAL_PROMPT.len()
        );

        // Verify capping behavior
        assert_eq!(capped_initial_prompt(), INITIAL_PROMPT);
        let huge_prompt = "word ".repeat(500);
        let capped = cap_prompt(&huge_prompt);
        assert!(capped.len() <= MAX_INITIAL_PROMPT_CHARS);
    }

    #[test]
    #[ignore = "requires local whisper model file"]
    fn test_stt_transcribe_local_model() {
        let default_model = jgt_core::config::Voice::default().stt_model;
        let model_path = match crate::models::check_whisper(&default_model) {
            Ok(p) => p,
            Err(e) => {
                println!("Skipping ignored test because default model '{default_model}' is missing: {e}");
                return;
            }
        };

        let stt = Stt::load(&model_path, 4).expect("failed to load whisper model");
        let silence = vec![0.0f32; 16_000];
        let transcript = stt.transcribe(&silence).expect("transcription failed");
        assert_eq!(transcript, "");
    }

    #[tokio::test]
    #[ignore = "requires local whisper model and piper"]
    async fn test_stt_transcribe_piper_speech() {
        use crate::tts::TtsEngine;
        use std::time::Instant;

        let default_model = jgt_core::config::Voice::default().stt_model;
        let model_path = match crate::models::check_whisper(&default_model) {
            Ok(p) => p,
            Err(e) => {
                println!("Skipping: whisper model '{default_model}' missing: {e}");
                return;
            }
        };
        let piper = match crate::tts::PiperCommand::new("piper", "en_US-lessac-medium") {
            Ok(p) => p,
            Err(e) => {
                println!("Skipping: piper missing: {e}");
                return;
            }
        };

        let stt = Stt::load(&model_path, 4).expect("failed to load whisper");

        let test_cases = [
            ("restart the coordinator session", "coordinator"),
            ("what is the status of the task", "status"),
            ("merge the branch", "branch"),
        ];

        println!("\n=== Testing default model ({default_model}) ===");
        for (phrase, expected_keyword) in test_cases {
            let pcm = piper.synthesize(phrase).await.expect("synthesis failed");
            let audio_16k = crate::resample::resample(pcm.sample_rate, 16_000, &pcm.to_f32())
                .expect("resample failed");

            let t0 = Instant::now();
            let transcript = stt.transcribe(&audio_16k).expect("transcription failed");
            let duration = t0.elapsed();
            println!("[{default_model}] '{phrase}' -> '{transcript}' in {duration:?}");

            let norm = normalize_transcript(&transcript);
            assert!(
                norm.contains(expected_keyword),
                "expected transcript for '{phrase}' to contain '{expected_keyword}', got '{transcript}'"
            );
        }

        // Benchmark small.en-q5_1 for comparison if present
        if let Ok(small_path) = crate::models::check_whisper("small.en-q5_1")
            && let Ok(small_stt) = Stt::load(&small_path, 4)
        {
            println!("\n=== Comparing with legacy small.en-q5_1 ===");
            for (phrase, _) in test_cases {
                let pcm = piper.synthesize(phrase).await.expect("synthesis failed");
                let audio_16k = crate::resample::resample(pcm.sample_rate, 16_000, &pcm.to_f32())
                    .expect("resample failed");

                let t0 = Instant::now();
                let transcript = small_stt.transcribe(&audio_16k).expect("transcription failed");
                let duration = t0.elapsed();
                println!("[small.en-q5_1] '{phrase}' -> '{transcript}' in {duration:?}");
            }
        }

        // Benchmark large-v3-turbo-q5_0 for comparison if present
        if let Ok(turbo_path) = crate::models::check_whisper("large-v3-turbo-q5_0")
            && let Ok(turbo_stt) = Stt::load(&turbo_path, 4)
        {
            println!("\n=== Comparing with large-v3-turbo-q5_0 ===");
            for (phrase, _) in test_cases {
                let pcm = piper.synthesize(phrase).await.expect("synthesis failed");
                let audio_16k = crate::resample::resample(pcm.sample_rate, 16_000, &pcm.to_f32())
                    .expect("resample failed");

                let t0 = Instant::now();
                let transcript = turbo_stt.transcribe(&audio_16k).expect("transcription failed");
                let duration = t0.elapsed();
                println!("[large-v3-turbo-q5_0] '{phrase}' -> '{transcript}' in {duration:?}");
            }
        }
    }

    #[tokio::test]
    #[ignore = "benchmark medium vs large-v3-turbo on same utterance"]
    async fn test_benchmark_medium_vs_large_v3_turbo_same_utterance() {
        use crate::audio::clean::clean_audio;
        use crate::tts::TtsEngine;
        use std::time::Instant;

        let medium_path = crate::models::check_whisper("medium.en-q5_0").expect("medium model missing");
        let turbo_path = crate::models::check_whisper("large-v3-turbo-q5_0").expect("turbo model missing");
        let piper = crate::tts::PiperCommand::new("piper", "en_US-lessac-medium").expect("piper missing");

        let phrase = "restart the coordinator session and check the backlog";
        let pcm = piper.synthesize(phrase).await.expect("synthesis failed");
        let audio_16k = crate::resample::resample(pcm.sample_rate, 16_000, &pcm.to_f32())
            .expect("resample failed");
        // Clean-up applied once (as done in listener.rs pipeline)
        let cleaned_audio = clean_audio(&audio_16k);

        let medium_stt = Stt::load(&medium_path, 4).expect("failed to load medium");
        let turbo_stt = Stt::load(&turbo_path, 4).expect("failed to load turbo");

        println!("\n========================================================");
        println!("Benchmark: SAME Utterance (beam 5, clean-up applied once, CPU only)");
        println!("Phrase: \"{}\"", phrase);
        println!("Audio duration: {:.2}s ({} samples)", cleaned_audio.len() as f32 / 16000.0, cleaned_audio.len());
        println!("========================================================");

        // Run medium at least 3 times
        let mut medium_times = Vec::new();
        let mut medium_transcript = String::new();
        for run in 1..=3 {
            let t0 = Instant::now();
            let transcript = medium_stt.transcribe(&cleaned_audio).expect("medium transcribe failed");
            let elapsed_ms = t0.elapsed().as_millis();
            println!("[medium.en-q5_0] Run {run}: {elapsed_ms} ms -> \"{transcript}\"");
            medium_times.push(elapsed_ms);
            medium_transcript = transcript;
        }

        // Run turbo at least 3 times
        let mut turbo_times = Vec::new();
        let mut turbo_transcript = String::new();
        for run in 1..=3 {
            let t0 = Instant::now();
            let transcript = turbo_stt.transcribe(&cleaned_audio).expect("turbo transcribe failed");
            let elapsed_ms = t0.elapsed().as_millis();
            println!("[large-v3-turbo-q5_0] Run {run}: {elapsed_ms} ms -> \"{transcript}\"");
            turbo_times.push(elapsed_ms);
            turbo_transcript = transcript;
        }

        let mut sorted_med = medium_times.clone();
        sorted_med.sort_unstable();
        let min_med = sorted_med[0];
        let median_med = sorted_med[sorted_med.len() / 2];

        let mut sorted_turbo = turbo_times.clone();
        sorted_turbo.sort_unstable();
        let min_turbo = sorted_turbo[0];
        let median_turbo = sorted_turbo[sorted_turbo.len() / 2];

        println!("\n------------------ Timing Summary ------------------");
        println!("Model                 | Min (ms) | Median (ms) | Transcript");
        println!("----------------------+----------+-------------+-------------------");
        println!("medium.en-q5_0        | {:8} | {:11} | {}", min_med, median_med, medium_transcript);
        println!("large-v3-turbo-q5_0   | {:8} | {:11} | {}", min_turbo, median_turbo, turbo_transcript);
        println!("----------------------------------------------------");
    }
}

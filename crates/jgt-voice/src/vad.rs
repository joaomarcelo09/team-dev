use std::collections::VecDeque;

pub const SAMPLE_RATE: u32 = 16_000;
pub const FRAME_SIZE: usize = 256; // 16 ms @ 16 kHz
pub const ONSET_FRAMES: usize = 10; // ~160 ms (about 150 ms)
pub const OFFSET_FRAMES: usize = 70; // ~1120 ms (about 1.1 s)
pub const PRE_ROLL_FRAMES: usize = 19; // ~304 ms (about 300 ms)
pub const PRE_ROLL_SAMPLES: usize = PRE_ROLL_FRAMES * FRAME_SIZE;
pub const MIN_SPEECH_SAMPLES: usize = 19 * FRAME_SIZE; // ~304 ms (drops shorter than 300 ms)
pub const MAX_UTTERANCE_SAMPLES: usize = 30 * SAMPLE_RATE as usize; // 480,000 samples (30 s)

/// Trait for voice activity detection on a single 16 kHz 256-sample frame.
pub trait VadDetector: Send {
    /// Returns a speech score in the range [0.0, 1.0].
    fn predict(&mut self, frame: &[f32]) -> f32;

    /// Resets the detector state.
    fn reset(&mut self);
}

/// The default VAD detector powered by earshot.
pub struct EarshotDetector {
    detector: Box<earshot::Detector>,
}

impl EarshotDetector {
    pub fn new() -> Self {
        Self {
            detector: earshot::Detector::default_boxed(),
        }
    }
}

impl Default for EarshotDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl VadDetector for EarshotDetector {
    fn predict(&mut self, frame: &[f32]) -> f32 {
        self.detector.predict_f32(frame)
    }

    fn reset(&mut self) {
        self.detector.reset();
    }
}

enum VadState {
    Idle,
    PendingOnset {
        speech_frames: usize,
        pending_audio: Vec<f32>,
    },
    Speaking {
        utterance: Vec<f32>,
        speech_samples: usize,
        consecutive_silence_frames: usize,
    },
}

/// A pure and testable VAD segmenter over 16 kHz mono f32 audio.
///
/// - Starts a segment after about 150 ms of speech
/// - Keeps about 300 ms of pre-roll audio before speech onset
/// - Ends a segment after about 1.1 s of silence
/// - Hard cap of 30 seconds
/// - Drops segments shorter than 300 ms of total speech
pub struct VadSegmenter<D: VadDetector = EarshotDetector> {
    detector: D,
    state: VadState,
    pre_roll: VecDeque<f32>,
    remainder: Vec<f32>,
    completed: VecDeque<Vec<f32>>,
    speech_threshold: f32,
    consecutive_speech_frames: usize,
    last_silence_wait_frames: usize,
}

impl VadSegmenter<EarshotDetector> {
    /// Creates a new segmenter using the default `EarshotDetector`.
    pub fn new() -> Self {
        Self::with_detector(EarshotDetector::new())
    }
}

impl Default for VadSegmenter<EarshotDetector> {
    fn default() -> Self {
        Self::new()
    }
}

impl<D: VadDetector> VadSegmenter<D> {
    /// Creates a new segmenter with a custom detector implementation.
    pub fn with_detector(detector: D) -> Self {
        Self {
            detector,
            state: VadState::Idle,
            pre_roll: VecDeque::with_capacity(PRE_ROLL_SAMPLES + FRAME_SIZE),
            remainder: Vec::with_capacity(FRAME_SIZE * 2),
            completed: VecDeque::new(),
            speech_threshold: 0.5,
            consecutive_speech_frames: 0,
            last_silence_wait_frames: 0,
        }
    }

    /// Sets the speech detection threshold (default 0.5).
    pub fn set_speech_threshold(&mut self, threshold: f32) {
        self.speech_threshold = threshold;
    }

    /// Resets the internal state and detector.
    pub fn reset(&mut self) {
        self.detector.reset();
        self.state = VadState::Idle;
        self.pre_roll.clear();
        self.remainder.clear();
        self.completed.clear();
        self.consecutive_speech_frames = 0;
        self.last_silence_wait_frames = 0;
    }

    /// Finishes any in-progress speech segment, returning the utterance if it meets `MIN_SPEECH_SAMPLES`.
    ///
    /// Resets the detector and segmenter state to `Idle`.
    pub fn finish(&mut self) -> Option<Vec<f32>> {
        let utterance = match std::mem::replace(&mut self.state, VadState::Idle) {
            VadState::Speaking {
                utterance,
                speech_samples,
                consecutive_silence_frames,
            } => {
                self.last_silence_wait_frames = consecutive_silence_frames;
                if speech_samples >= MIN_SPEECH_SAMPLES {
                    Some(utterance)
                } else {
                    None
                }
            }
            _ => self.completed.pop_front(),
        };
        self.detector.reset();
        self.pre_roll.clear();
        self.remainder.clear();
        self.completed.clear();
        self.consecutive_speech_frames = 0;
        self.state = VadState::Idle;
        utterance
    }

    /// Returns true if currently in the Speaking state.
    pub fn is_speaking(&self) -> bool {
        matches!(self.state, VadState::Speaking { .. })
    }

    /// Returns the silence wait in milliseconds of the last completed utterance.
    pub fn last_silence_wait_ms(&self) -> u32 {
        if self.last_silence_wait_frames > 0 {
            (self.last_silence_wait_frames as u32) * 16
        } else {
            (OFFSET_FRAMES as u32) * 16
        }
    }

    /// Returns the duration of the current continuous user speech in milliseconds (~16 ms per frame).
    pub fn continuous_speech_ms(&self) -> u32 {
        (self.consecutive_speech_frames as u32) * 16
    }

    /// Returns the number of consecutive speech frames detected.
    pub fn consecutive_speech_frames(&self) -> usize {
        self.consecutive_speech_frames
    }

    /// Pushes samples and invokes `on_frame` with current continuous speech ms after each processed frame.
    pub fn push_with_frame_callback<F>(&mut self, samples: &[f32], mut on_frame: F) -> Option<Vec<f32>>
    where
        F: FnMut(u32),
    {
        self.remainder.extend_from_slice(samples);

        while self.remainder.len() >= FRAME_SIZE {
            let frame: Vec<f32> = self.remainder.drain(..FRAME_SIZE).collect();
            let score = self.detector.predict(&frame);
            let is_speech = score >= self.speech_threshold;

            self.process_frame(frame, is_speech);
            on_frame(self.continuous_speech_ms());
        }

        self.completed.pop_front()
    }

    /// Pushes a slice of 16 kHz mono samples into the segmenter.
    ///
    /// Returns `Some(utterance)` when an utterance has finished.
    pub fn push(&mut self, samples: &[f32]) -> Option<Vec<f32>> {
        self.push_with_frame_callback(samples, |_| {})
    }

    fn process_frame(&mut self, frame: Vec<f32>, is_speech: bool) {
        if is_speech {
            self.consecutive_speech_frames += 1;
        } else {
            self.consecutive_speech_frames = 0;
        }

        match std::mem::replace(&mut self.state, VadState::Idle) {
            VadState::Idle => {
                if is_speech {
                    self.state = VadState::PendingOnset {
                        speech_frames: 1,
                        pending_audio: frame,
                    };
                } else {
                    self.append_pre_roll(&frame);
                    self.state = VadState::Idle;
                }
            }
            VadState::PendingOnset {
                mut speech_frames,
                mut pending_audio,
            } => {
                if is_speech {
                    speech_frames += 1;
                    pending_audio.extend_from_slice(&frame);

                    if speech_frames >= ONSET_FRAMES {
                        // Speech confirmed! Transition to Speaking with pre-roll prepended.
                        let mut utterance = Vec::with_capacity(
                            self.pre_roll.len() + pending_audio.len() + FRAME_SIZE * 64,
                        );
                        utterance.extend(self.pre_roll.drain(..));
                        let speech_samples = pending_audio.len();
                        utterance.extend(pending_audio);

                        self.state = VadState::Speaking {
                            utterance,
                            speech_samples,
                            consecutive_silence_frames: 0,
                        };
                    } else {
                        self.state = VadState::PendingOnset {
                            speech_frames,
                            pending_audio,
                        };
                    }
                } else {
                    // Speech stopped before onset threshold: transient noise, drop pending audio.
                    for chunk in pending_audio.chunks(FRAME_SIZE) {
                        self.append_pre_roll(chunk);
                    }
                    self.append_pre_roll(&frame);
                    self.state = VadState::Idle;
                }
            }
            VadState::Speaking {
                mut utterance,
                mut speech_samples,
                mut consecutive_silence_frames,
            } => {
                utterance.extend_from_slice(&frame);

                if is_speech {
                    speech_samples += FRAME_SIZE;
                    consecutive_silence_frames = 0;
                } else {
                    consecutive_silence_frames += 1;
                }

                // Check end-of-speech due to silence
                if consecutive_silence_frames >= OFFSET_FRAMES {
                    self.last_silence_wait_frames = consecutive_silence_frames;
                    if speech_samples >= MIN_SPEECH_SAMPLES {
                        self.completed.push_back(utterance);
                    }
                    self.detector.reset();
                    self.pre_roll.clear();
                    self.consecutive_speech_frames = 0;
                    self.state = VadState::Idle;
                    return;
                }

                // Check hard cap 30 seconds
                if utterance.len() >= MAX_UTTERANCE_SAMPLES {
                    self.last_silence_wait_frames = consecutive_silence_frames;
                    if speech_samples >= MIN_SPEECH_SAMPLES {
                        self.completed.push_back(utterance);
                    }
                    self.detector.reset();
                    self.pre_roll.clear();
                    self.consecutive_speech_frames = 0;
                    self.state = VadState::Idle;
                    return;
                }

                self.state = VadState::Speaking {
                    utterance,
                    speech_samples,
                    consecutive_silence_frames,
                };
            }
        }
    }

    fn append_pre_roll(&mut self, frame: &[f32]) {
        self.pre_roll.extend(frame);
        while self.pre_roll.len() > PRE_ROLL_SAMPLES {
            self.pre_roll.drain(..FRAME_SIZE.min(self.pre_roll.len()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockDetector {
        speech_map: Vec<bool>,
        index: usize,
    }

    impl MockDetector {
        fn new(speech_map: Vec<bool>) -> Self {
            Self {
                speech_map,
                index: 0,
            }
        }
    }

    impl VadDetector for MockDetector {
        fn predict(&mut self, _frame: &[f32]) -> f32 {
            let is_speech = if self.index < self.speech_map.len() {
                self.speech_map[self.index]
            } else {
                false
            };
            self.index += 1;
            if is_speech {
                1.0
            } else {
                0.0
            }
        }

        fn reset(&mut self) {
            // Keep index so it matches test timeline
        }
    }

    #[test]
    fn test_vad_utterance_detection() {
        // Timeline:
        // - 30 frames silence (480 ms) -> will fill 300 ms pre-roll
        // - 30 frames speech (480 ms) -> >150 ms onset, >300 ms min speech
        // - 75 frames silence (1200 ms) -> >1.1 s silence triggers end of utterance
        let mut speech_map = Vec::new();
        speech_map.extend(vec![false; 30]);
        speech_map.extend(vec![true; 30]);
        speech_map.extend(vec![false; 75]);

        let detector = MockDetector::new(speech_map);
        let mut segmenter = VadSegmenter::with_detector(detector);

        let dummy_frame = vec![0.0f32; FRAME_SIZE];
        let mut result = None;

        for _ in 0..140 {
            if let Some(utt) = segmenter.push(&dummy_frame) {
                result = Some(utt);
                break;
            }
        }

        assert!(result.is_some(), "Expected utterance to be emitted");
        let utt = result.unwrap();
        // Utterance should have:
        // pre-roll (~19 frames) + speech (30 frames) + trailing silence (70 frames) = ~119 frames = ~30,464 samples.
        let num_frames = utt.len() / FRAME_SIZE;
        assert!(
            (116..=122).contains(&num_frames),
            "Expected ~119 frames, got {num_frames}"
        );
        assert_eq!(segmenter.last_silence_wait_ms(), 1120);
    }

    #[test]
    fn test_vad_finish_while_speaking() {
        // 20 frames silence, then 25 frames speech (400 ms > 300 ms min speech)
        let mut speech_map = Vec::new();
        speech_map.extend(vec![false; 20]);
        speech_map.extend(vec![true; 25]);

        let detector = MockDetector::new(speech_map);
        let mut segmenter = VadSegmenter::with_detector(detector);

        let dummy_frame = vec![0.0f32; FRAME_SIZE];
        for _ in 0..45 {
            assert!(segmenter.push(&dummy_frame).is_none());
        }

        assert!(segmenter.is_speaking(), "Must be in Speaking state");
        let utt = segmenter.finish();
        assert!(utt.is_some(), "Utterance must be returned on finish");
        assert!(!segmenter.is_speaking(), "Must not be speaking after finish");
    }

    #[test]
    fn test_vad_finish_short_speech_dropped() {
        // 20 frames silence, then 12 frames speech (192 ms: >= onset 160 ms, but < min speech 304 ms)
        let mut speech_map = Vec::new();
        speech_map.extend(vec![false; 20]);
        speech_map.extend(vec![true; 12]);

        let detector = MockDetector::new(speech_map);
        let mut segmenter = VadSegmenter::with_detector(detector);

        let dummy_frame = vec![0.0f32; FRAME_SIZE];
        for _ in 0..32 {
            assert!(segmenter.push(&dummy_frame).is_none());
        }

        assert!(segmenter.is_speaking(), "Must be in Speaking state");
        let utt = segmenter.finish();
        assert!(utt.is_none(), "Utterance must be dropped because speech < min speech");
        assert!(!segmenter.is_speaking(), "Must not be speaking after finish");
    }

    #[test]
    fn test_vad_drops_short_speech() {
        // Speech of only 10 frames (160 ms): triggers onset (>=10 frames),
        // but total speech (160 ms) < 300 ms min speech threshold -> must be dropped!
        let mut speech_map = Vec::new();
        speech_map.extend(vec![false; 20]);
        speech_map.extend(vec![true; 10]);
        speech_map.extend(vec![false; 75]);

        let detector = MockDetector::new(speech_map);
        let mut segmenter = VadSegmenter::with_detector(detector);

        let dummy_frame = vec![0.0f32; FRAME_SIZE];
        let mut emitted = false;

        for _ in 0..110 {
            if segmenter.push(&dummy_frame).is_some() {
                emitted = true;
            }
        }

        assert!(!emitted, "Short speech (< 300 ms) must be dropped");
    }

    #[test]
    fn test_vad_drops_transient_below_onset() {
        // Speech of only 5 frames (80 ms) < 150 ms onset -> never triggers speaking state
        let mut speech_map = Vec::new();
        speech_map.extend(vec![false; 20]);
        speech_map.extend(vec![true; 5]);
        speech_map.extend(vec![false; 75]);

        let detector = MockDetector::new(speech_map);
        let mut segmenter = VadSegmenter::with_detector(detector);

        let dummy_frame = vec![0.0f32; FRAME_SIZE];
        let mut emitted = false;

        for _ in 0..105 {
            if segmenter.push(&dummy_frame).is_some() {
                emitted = true;
            }
        }

        assert!(!emitted, "Transient noise below onset must be dropped");
    }

    #[test]
    fn test_pause_shorter_than_offset_stays_one_segment() {
        // Timeline:
        // - 20 frames silence (320 ms)
        // - 30 frames speech (480 ms) -> triggers speaking state
        // - 50 frames silence (800 ms) -> pause shorter than OFFSET_FRAMES (70 frames / ~1.1s)
        // - 20 frames speech (320 ms) -> speech resumes
        // - 75 frames silence (1200 ms) -> silence >= OFFSET_FRAMES triggers end of utterance
        let mut speech_map = Vec::new();
        speech_map.extend(vec![false; 20]);
        speech_map.extend(vec![true; 30]);
        speech_map.extend(vec![false; 50]);
        speech_map.extend(vec![true; 20]);
        speech_map.extend(vec![false; 75]);

        let detector = MockDetector::new(speech_map);
        let mut segmenter = VadSegmenter::with_detector(detector);

        let dummy_frame = vec![0.0f32; FRAME_SIZE];
        let mut emitted_utterances = Vec::new();

        for _ in 0..200 {
            if let Some(utt) = segmenter.push(&dummy_frame) {
                emitted_utterances.push(utt);
            }
        }

        assert_eq!(
            emitted_utterances.len(),
            1,
            "Pause shorter than OFFSET_FRAMES must stay as one segment"
        );
        let utt = &emitted_utterances[0];
        // pre-roll (~19) + speech1 (30) + pause (50) + speech2 (20) + trailing silence (70) = 189 frames
        let num_frames = utt.len() / FRAME_SIZE;
        assert!(
            (186..=192).contains(&num_frames),
            "Expected ~189 frames, got {num_frames}"
        );
    }

    #[test]
    fn test_vad_hard_cap_30s() {
        // 20 frames silence, then continuous speech for 2000 frames (> 30s)
        // 30s = 1875 frames.
        let mut speech_map = Vec::new();
        speech_map.extend(vec![false; 20]);
        speech_map.extend(vec![true; 2000]);

        let detector = MockDetector::new(speech_map);
        let mut segmenter = VadSegmenter::with_detector(detector);

        let dummy_frame = vec![0.0f32; FRAME_SIZE];
        let mut result = None;

        for _ in 0..2020 {
            if let Some(utt) = segmenter.push(&dummy_frame) {
                result = Some(utt);
                break;
            }
        }

        assert!(result.is_some(), "Hard cap must emit utterance at 30s");
        let utt = result.unwrap();
        assert!(
            utt.len() >= MAX_UTTERANCE_SAMPLES,
            "Utterance should reach 30s cap"
        );
    }

    #[test]
    fn test_vad_continuous_speech_duration_and_frame_callback() {
        // 5 silence frames, 16 speech frames (256 ms), 1 silence frame, 2 speech frames (32 ms)
        let mut speech_map = vec![false; 5];
        speech_map.extend(vec![true; 16]);
        speech_map.push(false);
        speech_map.extend(vec![true; 2]);

        let detector = MockDetector::new(speech_map);
        let mut segmenter = VadSegmenter::with_detector(detector);
        let dummy_frame = vec![0.0f32; FRAME_SIZE];

        let mut observed_ms = Vec::new();

        for _ in 0..24 {
            segmenter.push_with_frame_callback(&dummy_frame, |ms| {
                observed_ms.push(ms);
            });
        }

        // First 5 silence frames: 0 ms
        assert_eq!(&observed_ms[0..5], &[0, 0, 0, 0, 0]);

        // Next 16 speech frames: 16, 32, ..., 256 ms
        let expected_speech: Vec<u32> = (1..=16).map(|i| i * 16).collect();
        assert_eq!(&observed_ms[5..21], &expected_speech[..]);
        assert_eq!(observed_ms[20], 256); // 16 frames = 256 ms >= 250 ms barge-in threshold

        // Silence frame resets to 0
        assert_eq!(observed_ms[21], 0);

        // Next 2 speech frames: 16, 32 ms
        assert_eq!(&observed_ms[22..24], &[16, 32]);
    }

    #[test]
    fn test_earshot_detector_creation() {
        let mut detector = EarshotDetector::new();
        let frame = vec![0.0f32; FRAME_SIZE];
        let score = detector.predict(&frame);
        assert!((0.0..=1.0).contains(&score));
    }
}

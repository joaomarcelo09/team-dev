//! Audio clean-up pre-processing applied before Whisper speech-to-text.
//!
//! Provides:
//! - 80 Hz 2nd-order Butterworth high-pass filter to eliminate DC offset and sub-audible low rumble.
//! - Peak gain normalisation targeting ~0.7 peak amplitude with capped gain and silence preservation.

use std::f32::consts::PI;

/// Target peak amplitude for speech normalisation (~0.7 FS, approx. -3 dBFS).
pub const TARGET_PEAK: f32 = 0.7;

/// Maximum linear gain boost applied during normalisation (+20 dB).
pub const DEFAULT_MAX_GAIN: f32 = 10.0;

/// Silence peak amplitude threshold (-40 dBFS). Audio with peak at or below this level
/// is considered silence/near-silence and is never amplified.
pub const DEFAULT_SILENCE_THRESHOLD: f32 = 0.01;

/// Default sample rate for STT audio (16 kHz).
pub const DEFAULT_SAMPLE_RATE: f32 = 16000.0;

/// Default high-pass filter cutoff frequency (80 Hz).
pub const DEFAULT_CUTOFF_HZ: f32 = 80.0;

/// Applies a 2nd-order Butterworth high-pass filter to `samples`.
///
/// Removes DC bias (mean offset) and eliminates sub-audible low-frequency rumble
/// (e.g. mic handling, HVAC, 50/60 Hz hum) while keeping speech frequencies flat (0 dB passband gain).
pub fn high_pass_filter(samples: &[f32], sample_rate: f32, cutoff_hz: f32) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }

    if sample_rate <= 0.0 || cutoff_hz <= 0.0 || cutoff_hz >= sample_rate * 0.5 {
        return samples.to_vec();
    }

    // Step 1: Remove constant DC offset (mean subtraction) to eliminate step transients.
    let mut finite_count = 0usize;
    let mut sum = 0.0f64;
    for &s in samples {
        if s.is_finite() {
            sum += s as f64;
            finite_count += 1;
        }
    }
    let mean = if finite_count > 0 {
        (sum / finite_count as f64) as f32
    } else {
        0.0f32
    };

    // Step 2: Bilinear transform design for Butterworth 2nd order HPF
    let omega = 2.0 * PI * cutoff_hz / sample_rate;
    let cos_w = omega.cos();
    let sin_w = omega.sin();
    let alpha = sin_w / (2.0 * std::f32::consts::FRAC_1_SQRT_2);

    let b0 = (1.0 + cos_w) / 2.0;
    let b1 = -(1.0 + cos_w);
    let b2 = (1.0 + cos_w) / 2.0;
    let a0 = 1.0 + alpha;
    let a1 = -2.0 * cos_w;
    let a2 = 1.0 - alpha;

    let b0_norm = b0 / a0;
    let b1_norm = b1 / a0;
    let b2_norm = b2 / a0;
    let a1_norm = a1 / a0;
    let a2_norm = a2 / a0;

    let mut out = Vec::with_capacity(samples.len());

    // Direct Form II Transposed filter states
    let mut s1 = 0.0f32;
    let mut s2 = 0.0f32;

    for &raw in samples {
        let x = if raw.is_finite() { raw - mean } else { 0.0 };
        let y = b0_norm * x + s1;
        s1 = b1_norm * x - a1_norm * y + s2;
        s2 = b2_norm * x - a2_norm * y;
        out.push(y);
    }

    out
}

/// Normalizes audio gain so that speech peak reaches `target_peak` (~0.7).
///
/// Rules:
/// - Silence / near-silence (peak <= `silence_threshold`): never amplified (gain = 1.0).
/// - Quiet speech (peak > `silence_threshold`): boosted towards `target_peak`, with gain capped at `max_gain`.
/// - Loud speech (peak > `target_peak`): scaled down towards `target_peak`, preventing clipping.
/// - All samples are clamped to `[-1.0, 1.0]` as a safety boundary.
pub fn normalize_gain(
    samples: &[f32],
    target_peak: f32,
    max_gain: f32,
    silence_threshold: f32,
) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }

    let mut current_peak = 0.0f32;
    for &s in samples {
        if s.is_finite() {
            let abs = s.abs();
            if abs > current_peak {
                current_peak = abs;
            }
        }
    }

    // Never amplify silence or near-silence.
    let gain = if current_peak <= silence_threshold || current_peak == 0.0 {
        1.0
    } else {
        let desired_gain = target_peak / current_peak;
        desired_gain.min(max_gain)
    };

    samples
        .iter()
        .map(|&s| {
            if s.is_finite() {
                (s * gain).clamp(-1.0, 1.0)
            } else {
                0.0
            }
        })
        .collect()
}

/// Applies standard voice clean-up before Whisper:
/// 1. 80 Hz 2nd-order Butterworth high-pass filter (removes DC bias and sub-audible low rumble).
/// 2. Peak gain normalisation (target peak 0.7, capped gain 10x, silence preserved).
pub fn clean_audio(samples: &[f32]) -> Vec<f32> {
    clean_audio_with_params(
        samples,
        DEFAULT_SAMPLE_RATE,
        DEFAULT_CUTOFF_HZ,
        TARGET_PEAK,
        DEFAULT_MAX_GAIN,
        DEFAULT_SILENCE_THRESHOLD,
    )
}

/// Applies audio clean-up with customizable filter and normalisation parameters.
pub fn clean_audio_with_params(
    samples: &[f32],
    sample_rate: f32,
    cutoff_hz: f32,
    target_peak: f32,
    max_gain: f32,
    silence_threshold: f32,
) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }
    let filtered = high_pass_filter(samples, sample_rate, cutoff_hz);
    normalize_gain(&filtered, target_peak, max_gain, silence_threshold)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peak_amplitude(audio: &[f32]) -> f32 {
        audio.iter().fold(0.0f32, |max, &s| max.max(s.abs()))
    }

    #[test]
    fn test_empty_input() {
        assert!(high_pass_filter(&[], 16000.0, 80.0).is_empty());
        assert!(normalize_gain(&[], 0.7, 10.0, 0.01).is_empty());
        assert!(clean_audio(&[]).is_empty());
    }

    #[test]
    fn test_silence_stays_silent() {
        // Pure digital silence (0.0) stays exactly 0.0
        let pure_silence = vec![0.0f32; 1600];
        let cleaned = clean_audio(&pure_silence);
        assert_eq!(cleaned.len(), 1600);
        for &s in &cleaned {
            assert_eq!(s, 0.0);
        }

        // Near-silence below threshold (e.g. peak = 0.005 < 0.01 threshold) must never be amplified
        let near_silence: Vec<f32> = (0..1600)
            .map(|i| (i as f32 * 300.0 * 2.0 * PI / 16000.0).sin() * 0.005)
            .collect();
        assert!(peak_amplitude(&near_silence) <= 0.005 + 1e-5);

        // Directly verifying normalize_gain does not amplify near-silence
        let normalized = normalize_gain(&near_silence, TARGET_PEAK, DEFAULT_MAX_GAIN, DEFAULT_SILENCE_THRESHOLD);
        assert_eq!(normalized, near_silence);

        // Through full clean_audio pipeline, near-silence stays below silence threshold and is not amplified towards 0.7
        let cleaned_near = clean_audio(&near_silence);
        let peak_after = peak_amplitude(&cleaned_near);
        assert!(
            peak_after < DEFAULT_SILENCE_THRESHOLD,
            "near-silence peak {peak_after} must remain below silence threshold ({DEFAULT_SILENCE_THRESHOLD})"
        );
    }

    #[test]
    fn test_quiet_speech_is_raised() {
        // Quiet speech: 500 Hz tone at 16 kHz with peak = 0.07 (approx -23 dBFS)
        let quiet_speech: Vec<f32> = (0..3200)
            .map(|i| (i as f32 * 500.0 * 2.0 * PI / 16000.0).sin() * 0.07)
            .collect();
        assert!((peak_amplitude(&quiet_speech) - 0.07).abs() < 1e-4);

        let cleaned = clean_audio(&quiet_speech);
        let peak_after = peak_amplitude(&cleaned);

        // Target peak is ~0.7. With 0.07 peak and max gain 10x, target 0.7 is reachable.
        assert!(
            (peak_after - TARGET_PEAK).abs() < 0.05,
            "quiet speech peak should be raised to near 0.7, got {peak_after}"
        );
    }

    #[test]
    fn test_capped_gain_on_very_quiet_speech() {
        // Very quiet speech: peak = 0.02 (> 0.01 silence threshold).
        // Desired gain = 0.7 / 0.02 = 35.0, which exceeds max_gain (10.0).
        let very_quiet: Vec<f32> = (0..3200)
            .map(|i| (i as f32 * 500.0 * 2.0 * PI / 16000.0).sin() * 0.02)
            .collect();

        let cleaned = clean_audio(&very_quiet);
        let peak_after = peak_amplitude(&cleaned);

        // Peak should be raised by at most max_gain (10x): ~0.02 * 10 = ~0.20
        assert!(
            (peak_after - 0.20).abs() < 0.03,
            "gain should be capped at max_gain 10x, expected peak ~0.20, got {peak_after}"
        );
        assert!(peak_after < TARGET_PEAK);
    }

    #[test]
    fn test_clean_audio_runs_once_total_gain_on_peak_0_011_stays_within_max_gain() {
        // Signal with peak 0.011 (just above the 0.01 silence threshold).
        let raw: Vec<f32> = (0..3200)
            .map(|i| (i as f32 * 500.0 * 2.0 * PI / 16000.0).sin() * 0.011)
            .collect();
        let initial_peak = peak_amplitude(&raw);
        assert!((initial_peak - 0.011).abs() < 1e-4);

        // One clean_audio pass:
        let pass1 = clean_audio(&raw);
        let peak_pass1 = peak_amplitude(&pass1);

        // Total gain must stay within ~DEFAULT_MAX_GAIN (10x), so peak stays ~0.11-0.13 and never ~0.70
        assert!(
            peak_pass1 <= 0.13,
            "single pass peak {peak_pass1} must remain near ~0.11 and not reach TARGET_PEAK (~0.7)"
        );

        // Verify that a second pass (the duplicate clean_audio bug) would have amplified
        // the signal a second time towards 0.70 (effective gain ~64x).
        let pass2 = clean_audio(&pass1);
        let peak_pass2 = peak_amplitude(&pass2);
        assert!(
            (peak_pass2 - TARGET_PEAK).abs() < 0.05,
            "second pass boosts peak to ~0.7 ({peak_pass2}), proving duplicate cleaning breaks gain cap"
        );
    }

    #[test]
    fn test_loud_is_not_clipped() {
        // Loud speech: peak = 0.95 (close to clipping)
        let loud_speech: Vec<f32> = (0..3200)
            .map(|i| (i as f32 * 400.0 * 2.0 * PI / 16000.0).sin() * 0.95)
            .collect();
        assert!((peak_amplitude(&loud_speech) - 0.95).abs() < 1e-4);

        let cleaned = clean_audio(&loud_speech);
        let peak_after = peak_amplitude(&cleaned);

        // Should be normalized down towards ~0.7 without clipping
        assert!(
            (peak_after - TARGET_PEAK).abs() < 0.05,
            "loud speech peak should be scaled down to ~0.7, got {peak_after}"
        );

        // All samples must strictly be within [-1.0, 1.0] and never clipped at rails
        for &s in &cleaned {
            assert!(s.abs() <= TARGET_PEAK + 1e-4);
        }

        // Even unclipped floating-point signal with peak 1.5 is normalized safely
        let over_unity: Vec<f32> = (0..1600)
            .map(|i| (i as f32 * 400.0 * 2.0 * PI / 16000.0).sin() * 1.5)
            .collect();
        let cleaned_over = clean_audio(&over_unity);
        let peak_over = peak_amplitude(&cleaned_over);
        assert!((peak_over - TARGET_PEAK).abs() < 0.05);
        for &s in &cleaned_over {
            assert!(s >= -1.0 && s <= 1.0);
        }
    }

    #[test]
    fn test_dc_removed() {
        // Pure constant DC offset of 0.8
        let pure_dc = vec![0.8f32; 3200];
        let cleaned = clean_audio(&pure_dc);

        // Constant DC is blocked by 80 Hz high-pass filter; steady state drops to 0.
        // Because DC is removed, the remaining energy is silence (< silence_threshold).
        let peak_after = peak_amplitude(&cleaned);
        assert!(
            peak_after < 1e-4,
            "DC offset must be removed, got residual peak {peak_after}"
        );

        // Speech tone (500 Hz, peak 0.2) riding on a severe DC bias (+0.5)
        let speech_with_dc: Vec<f32> = (0..4800)
            .map(|i| 0.5 + (i as f32 * 500.0 * 2.0 * PI / 16000.0).sin() * 0.2)
            .collect();

        let filtered = high_pass_filter(&speech_with_dc, 16000.0, 80.0);

        // In the filtered signal, average/mean should be ~0.0 (DC removed)
        let mean_filtered: f32 =
            filtered.iter().sum::<f32>() / filtered.len() as f32;
        assert!(
            mean_filtered.abs() < 0.001,
            "DC bias should be removed, mean is {mean_filtered}"
        );

        // And clean_audio should normalize the remaining speech tone to target peak ~0.7
        let cleaned = clean_audio(&speech_with_dc);
        let peak_cleaned = peak_amplitude(&cleaned);
        assert!(
            (peak_cleaned - TARGET_PEAK).abs() < 0.05,
            "speech with DC should have DC removed and peak normalized to ~0.7, got {peak_cleaned}"
        );
    }

    #[test]
    fn test_low_rumble_removed() {
        // 20 Hz low-frequency rumble (2 octaves below 80 Hz cutoff)
        let rumble_20hz: Vec<f32> = (0..8000)
            .map(|i| (i as f32 * 20.0 * 2.0 * PI / 16000.0).sin() * 0.5)
            .collect();

        // 500 Hz voice tone (well inside passband)
        let voice_500hz: Vec<f32> = (0..8000)
            .map(|i| (i as f32 * 500.0 * 2.0 * PI / 16000.0).sin() * 0.5)
            .collect();

        let filtered_rumble = high_pass_filter(&rumble_20hz, 16000.0, 80.0);
        let filtered_voice = high_pass_filter(&voice_500hz, 16000.0, 80.0);

        // Look at steady state (skip first 200 samples)
        let peak_rumble = peak_amplitude(&filtered_rumble[200..]);
        let peak_voice = peak_amplitude(&filtered_voice[200..]);

        // 20 Hz tone should be attenuated by > 20 dB (amplitude < 0.06 from 0.5)
        assert!(
            peak_rumble < 0.06,
            "20 Hz rumble should be heavily attenuated (< 0.06), got {peak_rumble}"
        );

        // 500 Hz speech tone should be preserved near 0.5
        assert!(
            (peak_voice - 0.5).abs() < 0.02,
            "500 Hz voice should be passed through (~0.5), got {peak_voice}"
        );
    }
}

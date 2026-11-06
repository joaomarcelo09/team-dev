pub mod capture;
pub mod playback;

use crate::error::VoiceError;

pub use capture::{downmix, start_capture, AudioCapture};
pub use playback::{AudioQueue, Player};

/// Finds the index of the first candidate whose name contains `target` as a case-insensitive substring.
///
/// Returns `VoiceError::DeviceNotFound` listing all available candidate names if no match is found.
pub fn find_device_index(target: &str, available: &[String]) -> Result<usize, VoiceError> {
    let target_trimmed = target.trim();
    let target_lower = target_trimmed.to_lowercase();
    for (idx, name) in available.iter().enumerate() {
        if name.to_lowercase().contains(&target_lower) {
            return Ok(idx);
        }
    }
    Err(VoiceError::DeviceNotFound {
        name: target_trimmed.to_string(),
        available: available.to_vec(),
    })
}

/// Helper to extract human-readable device name from a CPAL Device.
pub fn get_device_name(device: &cpal::Device) -> String {
    use cpal::traits::DeviceTrait;
    device
        .description()
        .map(|desc| desc.name().to_string())
        .unwrap_or_else(|_| device.to_string())
}

/// Selects a CPAL audio device matching `target` by case-insensitive substring from `devices`.
pub fn select_device(
    devices: Vec<cpal::Device>,
    target: &str,
) -> Result<cpal::Device, VoiceError> {
    let names: Vec<String> = devices.iter().map(get_device_name).collect();
    let idx = find_device_index(target, &names)?;
    Ok(devices.into_iter().nth(idx).expect("device index must be valid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_name_matching_case_insensitive_substring() {
        let available = vec![
            "Built-in Audio Analog Stereo".to_string(),
            "Echo-Cancel Sink (PipeWire)".to_string(),
            "USB Wireless Headset".to_string(),
        ];

        // Exact match
        assert_eq!(
            find_device_index("USB Wireless Headset", &available).unwrap(),
            2
        );

        // Case-insensitive substring match
        assert_eq!(find_device_index("pipewire", &available).unwrap(), 1);
        assert_eq!(find_device_index("PIPEWIRE", &available).unwrap(), 1);
        assert_eq!(find_device_index("echo-cancel", &available).unwrap(), 1);
        assert_eq!(find_device_index("analog", &available).unwrap(), 0);
        assert_eq!(find_device_index("  headset  ", &available).unwrap(), 2);
    }

    #[test]
    fn test_missing_device_error_lists_available_devices() {
        let available = vec![
            "Built-in Audio Analog Stereo".to_string(),
            "Echo-Cancel Sink (PipeWire)".to_string(),
        ];

        let err = find_device_index("bluetooth-mic", &available).unwrap_err();
        match &err {
            VoiceError::DeviceNotFound { name, available: list } => {
                assert_eq!(name, "bluetooth-mic");
                assert_eq!(list, &available);
            }
            other => panic!("expected DeviceNotFound error, got {other:?}"),
        }

        // Verify formatted error message lists the missing name and available device names
        let msg = err.to_string();
        assert!(msg.contains("bluetooth-mic"), "message should mention query: {msg}");
        assert!(
            msg.contains("Built-in Audio Analog Stereo"),
            "message should list available devices: {msg}"
        );
        assert!(
            msg.contains("Echo-Cancel Sink (PipeWire)"),
            "message should list available devices: {msg}"
        );
    }
}

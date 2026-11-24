//! Lab view model: the lab state label and one usage row per provider window.

use jgf_hub::usage::{HARD_PERCENT, LabState, Level, Provider, SOFT_PERCENT, Window};

/// The lab card: what the lab is doing and how much of each usage window it spent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabView {
    /// Lab state label, taken from [`LabState`]'s display form.
    pub state_label: String,
    /// One row per provider window, in the order given.
    pub usage: Vec<UsageRow>,
}

/// One provider's usage window, ready for the meter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRow {
    /// Provider name.
    pub provider: String,
    /// Used percentage, never below zero; an expired window is 0.
    pub percent: u64,
    /// Amount used in the window, as recorded.
    pub used: u64,
    /// Window limit.
    pub limit: u64,
    /// When the window resets, as plain text.
    pub resets_label: String,
    /// Throttle level implied by the percentage.
    pub level: Level,
}

/// Builds the lab view for `lab` and `windows` at `now` (unix seconds).
pub fn build_lab_view(lab: LabState, windows: &[(Provider, Window)], now: u64) -> LabView {
    LabView {
        state_label: lab.to_string(),
        usage: windows
            .iter()
            .map(|(provider, window)| usage_row(provider, window, now))
            .collect(),
    }
}

/// One usage row; an expired window reads 0% and is normal.
fn usage_row(provider: &Provider, window: &Window, now: u64) -> UsageRow {
    let expired = window.resets_at <= now;
    let percent = if expired { 0 } else { window.percent() };
    UsageRow {
        provider: provider.to_string(),
        percent,
        used: window.used,
        limit: window.limit,
        resets_label: resets_label(window.resets_at, now),
        level: level(percent),
    }
}

/// The throttle level for a used percentage.
fn level(percent: u64) -> Level {
    if percent >= HARD_PERCENT {
        Level::Hard
    } else if percent >= SOFT_PERCENT {
        Level::Soft
    } else {
        Level::Normal
    }
}

/// A plain-text reset label.
///
/// More than an hour away it counts down ("resets in 2h 05m"); otherwise, and once the window
/// expired, it shows the clock time in UTC ("resets at HH:MM UTC").
fn resets_label(resets_at: u64, now: u64) -> String {
    let remaining = resets_at.saturating_sub(now);
    if resets_at > now && remaining > 3_600 {
        format!(
            "resets in {}h {:02}m",
            remaining / 3_600,
            (remaining % 3_600) / 60
        )
    } else {
        format!("resets at {} UTC", utc_clock(resets_at))
    }
}

/// Formats unix seconds as `HH:MM` in UTC.
fn utc_clock(at: u64) -> String {
    let seconds = at % 86_400;
    format!("{:02}:{:02}", seconds / 3_600, (seconds % 3_600) / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(raw: &str) -> Provider {
        Provider::parse(raw).expect("valid provider")
    }

    #[test]
    fn state_label_matches_each_lab_state() {
        let cases = [
            LabState::Running,
            LabState::Throttled,
            LabState::Idle,
            LabState::PausedUntil(4_700),
        ];
        for state in cases {
            let view = build_lab_view(state, &[], 0);
            assert_eq!(view.state_label, state.to_string());
        }
        assert_eq!(
            build_lab_view(LabState::PausedUntil(4_700), &[], 0).state_label,
            "Paused until 01:18"
        );
    }

    #[test]
    fn percentages_map_to_levels() {
        let cases = [
            (0, Level::Normal),
            (79, Level::Normal),
            (80, Level::Soft),
            (94, Level::Soft),
            (95, Level::Hard),
            (120, Level::Hard),
        ];
        for (percent, expected) in cases {
            let window = Window::new(percent, 100, 1_000).expect("valid window");
            let view = build_lab_view(LabState::Running, &[(provider("claude"), window)], 0);
            assert_eq!(view.usage[0].percent, percent);
            assert_eq!(view.usage[0].level, expected);
        }
    }

    #[test]
    fn expired_window_reads_zero_percent() {
        let window = Window::new(100, 100, 600).expect("valid window");
        let view = build_lab_view(LabState::Running, &[(provider("claude"), window)], 700);
        assert_eq!(view.usage[0].percent, 0);
        assert_eq!(view.usage[0].level, Level::Normal);
        assert_eq!(view.usage[0].used, 100);
        assert_eq!(view.usage[0].resets_label, "resets at 00:10 UTC");
    }

    #[test]
    fn reset_labels_count_down_or_show_the_utc_clock() {
        let cases = [
            (7_500, "resets in 2h 05m"),
            (3_601, "resets in 1h 00m"),
            (600, "resets at 00:10 UTC"),
            (3_600, "resets at 01:00 UTC"),
        ];
        for (resets_at, expected) in cases {
            let window = Window::new(1, 100, resets_at).expect("valid window");
            let view = build_lab_view(LabState::Running, &[(provider("claude"), window)], 0);
            assert_eq!(view.usage[0].resets_label, expected);
        }
    }

    #[test]
    fn rows_keep_provider_names_and_order() {
        let windows = [
            (
                provider("claude"),
                Window::new(1, 10, 1_000).expect("valid"),
            ),
            (
                provider("opencode"),
                Window::new(2, 10, 1_000).expect("valid"),
            ),
        ];
        let view = build_lab_view(LabState::Idle, &windows, 0);
        let names: Vec<&str> = view.usage.iter().map(|row| row.provider.as_str()).collect();
        assert_eq!(names, ["claude", "opencode"]);
        assert_eq!(view.usage[1].used, 2);
        assert_eq!(view.usage[1].limit, 10);
    }
}

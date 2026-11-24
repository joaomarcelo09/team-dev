//! Window styling: Dark Factory uses iced's built-in dark theme.
//!
//! No custom palette, assets or images: widgets take their colors from the built-in theme.

use iced::{
    Color, Theme, border,
    widget::{container, progress_bar, text},
};
use jgf_hub::usage::{LabState, Level};

/// The built-in dark theme.
pub fn dark() -> Theme {
    Theme::Dark
}

/// One Board column: a subtle panel with a border and rounded corners.
pub fn board_column(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(palette.background.weak.color.into()),
        border: border::rounded(6),
        ..container::Style::default()
    }
}

/// The small project badge pill shown on a Board card.
pub fn project_badge(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(palette.primary.weak.color.into()),
        border: border::rounded(9),
        text_color: Some(palette.primary.weak.text),
        ..container::Style::default()
    }
}

/// A muted style for secondary lines such as an empty column.
pub fn muted_text(theme: &Theme) -> text::Style {
    let palette = theme.extended_palette();
    text::Style {
        color: Some(palette.background.weak.text),
    }
}

/// The colour of a lab state dot.
pub fn lab_state_color(theme: &Theme, state: LabState) -> Color {
    let palette = theme.extended_palette();
    match state {
        LabState::Running => palette.success.base.color,
        LabState::Throttled => palette.warning.base.color,
        LabState::PausedUntil(_) => palette.danger.base.color,
        LabState::Idle => palette.background.strong.color,
    }
}

/// The colour of a usage level: normal, soft (80%) or hard (95%).
pub fn level_color(theme: &Theme, level: Level) -> Color {
    let palette = theme.extended_palette();
    match level {
        Level::Normal => palette.success.base.color,
        Level::Soft => palette.warning.base.color,
        Level::Hard => palette.danger.base.color,
    }
}

/// A small round dot filled with the colour of `state`.
pub fn lab_state_dot(state: LabState) -> impl Fn(&Theme) -> container::Style {
    move |theme| container::Style {
        background: Some(lab_state_color(theme, state).into()),
        border: border::rounded(7),
        ..container::Style::default()
    }
}

/// A progress bar tinted for `level`.
pub fn level_progress_bar(level: Level) -> impl Fn(&Theme) -> progress_bar::Style {
    move |theme| {
        let palette = theme.extended_palette();
        progress_bar::Style {
            background: palette.background.strong.color.into(),
            bar: level_color(theme, level).into(),
            border: border::rounded(5),
        }
    }
}

/// The percentage text of a usage meter, tinted for `level`.
pub fn level_text(level: Level) -> impl Fn(&Theme) -> text::Style {
    move |theme| text::Style {
        color: Some(level_color(theme, level)),
    }
}

/// A red style for a rejected settings edit.
pub fn error_text(theme: &Theme) -> text::Style {
    let palette = theme.extended_palette();
    text::Style {
        color: Some(palette.danger.base.color),
    }
}

/// A green style for the short "Saved" note.
pub fn success_text(theme: &Theme) -> text::Style {
    let palette = theme.extended_palette();
    text::Style {
        color: Some(palette.success.base.color),
    }
}

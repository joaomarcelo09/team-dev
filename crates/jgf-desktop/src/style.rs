//! Factory-specific widget styling, using the active application theme's palette.

use iced::{
    Color, Theme, border,
    widget::{progress_bar, text},
};
use jgf_hub::usage::{LabState, Level};

/// A muted style for secondary lines such as an empty column.
pub fn muted_text(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(jg_ui::muted(theme)),
    }
}

/// Semantic state colors shared by Board task and coordinator-role indicators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskRoleState {
    Running,
    WaitingForUser,
    Failed,
    Done,
    Queued,
}

/// The colour of a task or role state.
pub fn task_role_state_color(theme: &Theme, state: TaskRoleState) -> Color {
    let palette = jg_ui::ext(theme);
    match state {
        TaskRoleState::Running => palette.success.weak.color,
        TaskRoleState::Done => palette.success.base.color,
        TaskRoleState::WaitingForUser => palette.warning.base.color,
        TaskRoleState::Failed => palette.danger.base.color,
        TaskRoleState::Queued => palette.primary.base.color,
    }
}

/// The colour of a lab state dot.
pub fn lab_state_color(theme: &Theme, state: LabState) -> Color {
    let palette = jg_ui::ext(theme);
    match state {
        LabState::Running => palette.success.base.color,
        LabState::Throttled => palette.warning.base.color,
        LabState::PausedUntil(_) => palette.danger.base.color,
        LabState::Idle => jg_ui::muted(theme),
    }
}

/// The colour of a usage level: normal, soft (80%) or hard (95%).
pub fn level_color(theme: &Theme, level: Level) -> Color {
    let palette = jg_ui::ext(theme);
    match level {
        Level::Normal => palette.success.base.color,
        Level::Soft => palette.warning.base.color,
        Level::Hard => palette.danger.base.color,
    }
}

/// A progress bar tinted for `level`.
pub fn level_progress_bar(level: Level) -> impl Fn(&Theme) -> progress_bar::Style {
    move |theme| {
        let palette = jg_ui::ext(theme);
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
    text::Style {
        color: Some(jg_ui::ext(theme).danger.base.color),
    }
}

/// A green style for the short "Saved" note.
pub fn success_text(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(jg_ui::ext(theme).success.base.color),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_and_role_states_follow_the_semantic_palette_in_every_theme() {
        let themes = [
            jg_ui::dark(),
            jg_ui::light(),
            jg_ui::tokyo_night(),
            jg_ui::dracula(),
            jg_ui::violet(),
        ];

        for theme in themes {
            let palette = jg_ui::ext(&theme);
            let running = task_role_state_color(&theme, TaskRoleState::Running);
            let waiting = task_role_state_color(&theme, TaskRoleState::WaitingForUser);
            let failed = task_role_state_color(&theme, TaskRoleState::Failed);
            let done = task_role_state_color(&theme, TaskRoleState::Done);
            let queued = task_role_state_color(&theme, TaskRoleState::Queued);

            assert_eq!(running, palette.success.weak.color);
            assert_eq!(done, palette.success.base.color);
            assert_eq!(waiting, palette.warning.base.color);
            assert_eq!(failed, palette.danger.base.color);
            assert_eq!(queued, palette.primary.base.color);
            assert_ne!(running, waiting);
            assert_ne!(running, failed);
            assert_ne!(running, done);
            assert_ne!(waiting, failed);
            assert_ne!(waiting, done);
            assert_ne!(failed, done);
            assert_ne!(queued, running);
            assert_ne!(queued, waiting);
            assert_ne!(queued, failed);
            assert_ne!(queued, done);
        }
    }
}

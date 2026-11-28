//! Desktop font settings and styles that depend on jgt-core types.

use iced::{Color, Font, Theme};
use jgt_core::{NotifyLevel, SessionStatus, TaskPriority, config::Desktop as DesktopConfig};

pub use jg_ui::*;

/// The UI font family: `[desktop] font` in config.toml, else the first installed of a few
/// common sans-serif families. Static fonts only: iced cannot pick weights out of variable
/// fonts (Inter, Adwaita Sans, …), so bold text would fall back to a monospace font.
pub fn ui_font() -> Font {
    static FAMILY: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    let family = FAMILY.get_or_init(|| {
        let configured = jgt_core::config::Config::load().desktop.font;
        let candidates = configured.into_iter().chain(
            ["Noto Sans", "DejaVu Sans", "Liberation Sans", "Open Sans", "Roboto"].map(String::from),
        );
        let installed = installed_families();
        candidates
            .into_iter()
            .find(|c| installed.iter().any(|f| f.eq_ignore_ascii_case(c)))
            .map(|f| &*Box::leak(f.into_boxed_str()))
    });
    match family {
        Some(name) => Font::with_name(name),
        None => Font::DEFAULT,
    }
}

/// Chat text size in pixels, set from `[desktop] text_size`; every other text size scales with it.
static TEXT_SIZE: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(DesktopConfig::DEFAULT_TEXT_SIZE);

pub fn set_text_size(size: u16) {
    TEXT_SIZE.store(size, std::sync::atomic::Ordering::Relaxed);
}

/// A text size from the layout (designed around 14px chat text), scaled to the user's choice.
pub fn fs(size: u16) -> f32 {
    f32::from(size) * f32::from(TEXT_SIZE.load(std::sync::atomic::Ordering::Relaxed)) / 14.0
}

fn installed_families() -> Vec<String> {
    std::process::Command::new("fc-list")
        .args([":", "family"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .flat_map(|l| l.split(','))
                .map(|f| f.trim().to_string())
                .collect()
        })
        .unwrap_or_default()
}

pub fn status_color(theme: &Theme, status: SessionStatus) -> Color {
    let p = ext(theme);
    match status {
        SessionStatus::Active => p.primary.base.color,
        SessionStatus::Listening => p.success.base.color,
        SessionStatus::Idle => p.success.weak.color,
        SessionStatus::Disconnected => p.warning.base.color,
        SessionStatus::Ended => muted(theme),
    }
}

pub fn level_color(theme: &Theme, level: NotifyLevel) -> Color {
    let p = ext(theme);
    match level {
        NotifyLevel::Info => p.primary.base.color,
        NotifyLevel::Success => p.success.base.color,
        NotifyLevel::Warning => p.warning.base.color,
        NotifyLevel::Error => p.danger.base.color,
    }
}

pub fn priority_color(theme: &Theme, priority: TaskPriority) -> Color {
    let p = ext(theme);
    match priority {
        TaskPriority::High => p.danger.base.color,
        TaskPriority::Normal => p.primary.base.color,
        TaskPriority::Low => muted(theme),
    }
}

//! Colors and widget styles. Everything derives from two custom palettes so light and dark
//! mode stay consistent.

use iced::{
    Background, Border, Color, Font, Shadow, Theme, border,
    overlay::menu,
    theme::{
        Palette,
        palette::{self, Extended},
    },
    widget::{button, container, pick_list, text_editor, text_input},
};
use jgt_core::{NotifyLevel, SessionStatus, TaskPriority, config::Desktop as DesktopConfig};

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

pub fn dark() -> Theme {
    Theme::custom(
        "jg-team-dev Dark",
        Palette {
            background: rgb(0x16181d),
            text: rgb(0xe6e8ee),
            primary: rgb(0x5b8cff),
            success: rgb(0x3fb96f),
            warning: rgb(0xe0a63a),
            danger: rgb(0xe5534b),
        },
    )
}

pub fn light() -> Theme {
    Theme::custom(
        "jg-team-dev Light",
        Palette {
            background: rgb(0xf6f7f9),
            text: rgb(0x1d2129),
            primary: rgb(0x2f6bed),
            success: rgb(0x23944f),
            warning: rgb(0xb7791f),
            danger: rgb(0xd1383d),
        },
    )
}

pub fn tokyo_night() -> Theme {
    Theme::custom_with_fn(
        "Tokyo Night",
        Palette {
            background: rgb(0x1a1b26),
            text: rgb(0xc0caf5),
            primary: rgb(0x7aa2f7),
            success: rgb(0x9ece6a),
            warning: rgb(0xe0af68),
            danger: rgb(0xf7768e),
        },
        |palette| {
            let mut ext = Extended::generate(palette);
            ext.background.weak = palette::Pair::new(rgb(0x24283b), palette.text);
            ext
        },
    )
}

pub fn dracula() -> Theme {
    Theme::custom_with_fn(
        "Dracula",
        Palette {
            background: rgb(0x282a36),
            text: rgb(0xf8f8f2),
            primary: rgb(0xbd93f9),
            success: rgb(0x50fa7b),
            warning: rgb(0xffb86c),
            danger: rgb(0xff5555),
        },
        |palette| {
            let mut ext = Extended::generate(palette);
            ext.background.weak = palette::Pair::new(rgb(0x44475a), palette.text);
            ext
        },
    )
}

pub fn violet() -> Theme {
    Theme::custom_with_fn(
        "JG Violet",
        Palette {
            background: rgb(0x111319),
            text: rgb(0xe2e2eb),
            primary: rgb(0x7c3aed),
            success: rgb(0x4edea3),
            warning: rgb(0xe0a63a),
            danger: rgb(0xffb4ab),
        },
        |palette| {
            let mut ext = Extended::generate(palette);
            ext.background.weak = palette::Pair::new(rgb(0x1e1f26), palette.text);
            ext.background.strong = palette::Pair::new(rgb(0x33343b), palette.text);
            ext
        },
    )
}

#[allow(dead_code)]
pub mod tokens {
    use iced::Color;
    use super::rgb;

    /// Sidebar background surface (darkest).
    pub const SURFACE_LOWEST: Color = rgb(0x0c0e14);
    /// Header strips background surface.
    pub const SURFACE_LOW: Color = rgb(0x191b22);
    /// Cards and pills surface.
    pub const SURFACE_CARD: Color = rgb(0x1e1f26);
    /// Hover state surface.
    pub const SURFACE_HOVER: Color = rgb(0x282a30);
    /// Highest surface (elevated elements, borders).
    pub const SURFACE_HIGHEST: Color = rgb(0x33343b);
    /// Bright surface.
    pub const SURFACE_BRIGHT: Color = rgb(0x373940);

    /// Muted body and helper text.
    pub const TEXT_MUTED: Color = rgb(0xccc3d8);
    /// Component outlines and inactive borders.
    pub const OUTLINE: Color = rgb(0x958da1);
    /// Separator lines and dividers.
    pub const SEPARATOR: Color = rgb(0x4a4455);

    /// Primary accent for text and icons.
    pub const ACCENT: Color = rgb(0xd2bbff);
    /// Foreground text on primary background elements.
    pub const ON_PRIMARY: Color = rgb(0xede0ff);
    /// Secondary accent color.
    pub const SECONDARY: Color = rgb(0xc0c1ff);

    pub const RADIUS_SM: f32 = 4.0;
    pub const RADIUS_MD: f32 = 8.0;
    pub const RADIUS_PILL: f32 = 12.0;
}

#[allow(unused_imports)]
pub use tokens::{RADIUS_MD, RADIUS_PILL, RADIUS_SM};

const fn rgb(hex: u32) -> Color {
    Color::from_rgb8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

fn ext(theme: &Theme) -> &Extended {
    theme.extended_palette()
}

pub fn muted(theme: &Theme) -> Color {
    let p = ext(theme);
    Color { a: 0.62, ..p.background.base.text }
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

pub fn sidebar(theme: &Theme) -> container::Style {
    let p = ext(theme);
    let bg = if theme.palette().background == rgb(0x111319) {
        tokens::SURFACE_LOWEST
    } else {
        p.background.weak.color.scale_alpha(if p.is_dark { 0.35 } else { 0.6 })
    };
    container::Style {
        background: Some(bg.into()),
        ..Default::default()
    }
}

pub fn header(theme: &Theme) -> container::Style {
    let p = ext(theme);
    let bg = if theme.palette().background == rgb(0x111319) {
        tokens::SURFACE_LOW
    } else if p.is_dark {
        p.background.weak.color.scale_alpha(0.5)
    } else {
        p.background.weak.color.scale_alpha(0.7)
    };
    container::Style {
        background: Some(bg.into()),
        text_color: Some(p.background.base.text),
        border: Border { color: p.background.strong.color.scale_alpha(0.5), width: 0.0, radius: 0.0.into() },
        ..Default::default()
    }
}

/// The top-level 56px app header: low surface background following the theme palette.
pub fn app_header(theme: &Theme) -> container::Style {
    let p = ext(theme);
    let bg = if p.is_dark {
        p.background.weak.color.scale_alpha(0.5)
    } else {
        p.background.weak.color.scale_alpha(0.7)
    };
    container::Style {
        background: Some(bg.into()),
        text_color: Some(p.background.base.text),
        ..Default::default()
    }
}

/// A compact status/summary pill with rounded pill edges.
pub fn summary_pill(theme: &Theme) -> container::Style {
    let p = ext(theme);
    container::Style {
        background: Some(p.background.strong.color.scale_alpha(if p.is_dark { 0.45 } else { 0.25 }).into()),
        border: border::rounded(RADIUS_PILL),
        text_color: Some(p.background.base.text),
        ..Default::default()
    }
}

/// Pick list style for the workspace selector pill in the app header.
pub fn workspace_selector(theme: &Theme, status: pick_list::Status) -> pick_list::Style {
    let p = ext(theme);
    let bg = match status {
        pick_list::Status::Hovered | pick_list::Status::Opened { .. } => {
            p.background.strong.color.scale_alpha(if p.is_dark { 0.45 } else { 0.35 })
        }
        pick_list::Status::Active => {
            p.background.weak.color.scale_alpha(if p.is_dark { 0.45 } else { 0.35 })
        }
    };
    pick_list::Style {
        text_color: p.background.base.text,
        placeholder_color: muted(theme),
        handle_color: p.primary.base.color,
        background: Background::Color(bg),
        border: border::rounded(RADIUS_MD),
    }
}

/// Dropdown menu style for the workspace selector.
pub fn workspace_menu(theme: &Theme) -> menu::Style {
    let p = ext(theme);
    menu::Style {
        background: Background::Color(if p.is_dark { p.background.weak.color } else { Color::WHITE }),
        border: Border {
            color: p.background.strong.color.scale_alpha(if p.is_dark { 0.5 } else { 0.3 }),
            width: 1.0,
            radius: RADIUS_MD.into(),
        },
        text_color: p.background.base.text,
        selected_text_color: p.primary.base.text,
        selected_background: Background::Color(p.primary.base.color),
        shadow: Shadow::default(),
    }
}

pub fn session_item(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let p = ext(theme);
        let background = match (selected, status) {
            (true, _) => Some(p.primary.weak.color.scale_alpha(if p.is_dark { 0.35 } else { 0.25 })),
            (false, button::Status::Hovered) => Some(p.background.strong.color.scale_alpha(0.35)),
            _ => None,
        };
        button::Style {
            background: background.map(Background::Color),
            text_color: p.background.base.text,
            border: border::rounded(RADIUS_MD),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

pub fn bubble_agent(theme: &Theme) -> container::Style {
    let p = ext(theme);
    container::Style {
        background: Some(if p.is_dark { p.background.weak.color } else { Color::WHITE }.into()),
        border: Border { color: p.background.strong.color.scale_alpha(0.4), width: 1.0, radius: RADIUS_PILL.into() },
        text_color: Some(p.background.base.text),
        ..Default::default()
    }
}

pub fn bubble_user(theme: &Theme) -> container::Style {
    let p = ext(theme);
    container::Style {
        background: Some(p.primary.base.color.into()),
        border: border::rounded(RADIUS_PILL),
        text_color: Some(p.primary.base.text),
        ..Default::default()
    }
}

pub fn bubble_failed(theme: &Theme) -> container::Style {
    let p = ext(theme);
    container::Style {
        background: Some(p.danger.weak.color.scale_alpha(0.5).into()),
        border: Border { color: p.danger.base.color, width: 1.0, radius: RADIUS_PILL.into() },
        text_color: Some(p.background.base.text),
        ..Default::default()
    }
}

/// A card with a colored accent border (notifications, questions, permissions).
pub fn card(accent: Color) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        let p = ext(theme);
        container::Style {
            background: Some(
                if p.is_dark { p.background.weak.color } else { Color::WHITE }.into(),
            ),
            border: Border { color: accent, width: 1.5, radius: RADIUS_PILL.into() },
            text_color: Some(p.background.base.text),
            ..Default::default()
        }
    }
}

pub fn banner(accent: Color) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        let p = ext(theme);
        container::Style {
            background: Some(accent.scale_alpha(if p.is_dark { 0.16 } else { 0.12 }).into()),
            border: Border { color: accent.scale_alpha(0.5), width: 1.0, radius: RADIUS_MD.into() },
            text_color: Some(p.background.base.text),
            ..Default::default()
        }
    }
}

pub fn badge(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        let p = ext(theme);
        let text_color = if color == p.primary.base.color {
            p.primary.base.text
        } else if color == p.warning.base.color {
            p.warning.base.text
        } else {
            Color::WHITE
        };
        container::Style {
            background: Some(color.into()),
            border: border::rounded(9),
            text_color: Some(text_color),
            ..Default::default()
        }
    }
}

pub fn pill(theme: &Theme) -> container::Style {
    let p = ext(theme);
    container::Style {
        background: Some(p.background.strong.color.scale_alpha(0.45).into()),
        border: border::rounded(6),
        text_color: Some(p.background.base.text),
        ..Default::default()
    }
}

pub fn dot(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style { background: Some(color.into()), border: border::rounded(5), ..Default::default() }
}

pub fn composer_box(theme: &Theme) -> container::Style {
    let p = ext(theme);
    container::Style {
        background: Some(p.background.base.color.into()),
        border: Border { color: p.background.strong.color.scale_alpha(0.5), width: 1.0, radius: 0.0.into() },
        ..Default::default()
    }
}

pub fn editor(theme: &Theme, status: text_editor::Status) -> text_editor::Style {
    let mut style = text_editor::default(theme, status);
    style.border = style.border.rounded(10);
    style
}

pub fn read_only_editor(theme: &Theme, status: text_editor::Status) -> text_editor::Style {
    let p = ext(theme);
    let mut style = text_editor::default(theme, status);
    style.background = Background::Color(Color::TRANSPARENT);
    style.border = Border {
        color: p.background.strong.color.scale_alpha(0.5),
        width: 1.0,
        radius: RADIUS_MD.into(),
    };
    style.value = p.background.base.text;
    style.selection = p.primary.base.color.scale_alpha(0.35);
    style
}

pub fn read_only_user_editor(theme: &Theme, status: text_editor::Status) -> text_editor::Style {
    let p = ext(theme);
    let mut style = text_editor::default(theme, status);
    style.background = Background::Color(Color::TRANSPARENT);
    style.border = Border {
        color: p.primary.base.text.scale_alpha(0.4),
        width: 1.0,
        radius: RADIUS_MD.into(),
    };
    style.value = p.primary.base.text;
    style.selection = p.primary.base.text.scale_alpha(0.35);
    style
}

pub fn input(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let mut style = text_input::default(theme, status);
    style.border = style.border.rounded(RADIUS_MD);
    style
}

/// Small flat button (icons, secondary actions).
pub fn ghost(theme: &Theme, status: button::Status) -> button::Style {
    let p = ext(theme);
    let background = match status {
        button::Status::Hovered | button::Status::Pressed => Some(p.background.strong.color.scale_alpha(0.45).into()),
        _ => None,
    };
    button::Style {
        background,
        text_color: match status {
            button::Status::Disabled => muted(theme).scale_alpha(0.6),
            _ => p.background.base.text,
        },
        border: border::rounded(6),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Flat button for inside user bubbles (accented by primary color).
pub fn ghost_on_primary(theme: &Theme, status: button::Status) -> button::Style {
    let p = ext(theme);
    let background = match status {
        button::Status::Hovered | button::Status::Pressed => Some(p.primary.base.text.scale_alpha(0.15).into()),
        _ => None,
    };
    button::Style {
        background,
        text_color: match status {
            button::Status::Disabled => p.primary.base.text.scale_alpha(0.4),
            _ => p.primary.base.text,
        },
        border: border::rounded(6),
        shadow: Shadow::default(),
        snap: true,
    }
}

pub fn pill_button(theme: &Theme, status: button::Status) -> button::Style {
    let mut style = button::secondary(theme, status);
    style.border = style.border.rounded(RADIUS_MD);
    style
}

pub fn primary_button(theme: &Theme, status: button::Status) -> button::Style {
    let mut style = button::primary(theme, status);
    style.border = style.border.rounded(RADIUS_MD);
    style
}

pub fn success_button(theme: &Theme, status: button::Status) -> button::Style {
    let mut style = button::success(theme, status);
    style.border = style.border.rounded(RADIUS_MD);
    style
}

pub fn danger_button(theme: &Theme, status: button::Status) -> button::Style {
    let mut style = button::danger(theme, status);
    style.border = style.border.rounded(RADIUS_MD);
    style
}

/// Container for segmented controls on a card surface with rounded corners.
pub fn segmented_container(theme: &Theme) -> container::Style {
    let p = ext(theme);
    let bg = if theme.palette().background == rgb(0x111319) {
        tokens::SURFACE_CARD
    } else if p.is_dark {
        p.background.weak.color.scale_alpha(0.6)
    } else {
        p.background.weak.color.scale_alpha(0.7)
    };
    container::Style {
        background: Some(bg.into()),
        border: border::rounded(RADIUS_MD),
        ..Default::default()
    }
}

/// Segment button in a segmented control: active segment is filled with primary / ON_PRIMARY,
/// inactive is muted on the container's card surface.
pub fn segment_button(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let p = ext(theme);
        let is_violet = theme.palette().background == rgb(0x111319);
        let (background, text_color) = if active {
            let bg = match status {
                button::Status::Hovered => p.primary.base.color.scale_alpha(0.9),
                button::Status::Pressed => p.primary.strong.color,
                _ => p.primary.base.color,
            };
            let text = if is_violet {
                tokens::ON_PRIMARY
            } else {
                p.primary.base.text
            };
            (Some(bg.into()), text)
        } else {
            let bg = match status {
                button::Status::Hovered | button::Status::Pressed => {
                    Some(p.background.strong.color.scale_alpha(0.35).into())
                }
                _ => None,
            };
            let text = match status {
                button::Status::Disabled => muted(theme).scale_alpha(0.4),
                button::Status::Hovered | button::Status::Pressed => p.background.base.text,
                _ => if is_violet { tokens::TEXT_MUTED } else { muted(theme) },
            };
            (bg, text)
        };
        button::Style {
            background,
            text_color,
            border: border::rounded(RADIUS_SM),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

// ── board ───────────────────────────────────────────────────────────────────

/// The Chat | Board tabs in a Coordinator's header, and the IDE file tabs.
pub fn tab(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let p = ext(theme);
        let mut style = ghost(theme, status);
        if active {
            style.background = Some(p.primary.weak.color.scale_alpha(if p.is_dark { 0.35 } else { 0.25 }).into());
            style.text_color = p.background.base.text;
        }
        style
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

/// A Board column: a soft lane behind the cards.
pub fn board_column(theme: &Theme) -> container::Style {
    let p = ext(theme);
    container::Style {
        background: Some(p.background.weak.color.scale_alpha(if p.is_dark { 0.35 } else { 0.6 }).into()),
        border: border::rounded(10),
        text_color: Some(p.background.base.text),
        ..Default::default()
    }
}

/// A task card; the open one has a primary border, a waiting one a warning border.
pub fn task_card(open: bool, waiting: bool) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        let p = ext(theme);
        let border = if open {
            Border { color: p.primary.base.color, width: 1.5, radius: RADIUS_MD.into() }
        } else if waiting {
            Border { color: p.warning.base.color, width: 1.5, radius: RADIUS_MD.into() }
        } else {
            Border { color: p.background.strong.color.scale_alpha(0.4), width: 1.0, radius: RADIUS_MD.into() }
        };
        container::Style {
            background: Some(if p.is_dark { p.background.weak.color } else { Color::WHITE }.into()),
            border,
            text_color: Some(p.background.base.text),
            ..Default::default()
        }
    }
}

/// The line where a dragged card would land.
pub fn drop_line(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(ext(theme).primary.base.color.into()),
        border: Border { radius: 2.0.into(), ..Default::default() },
        ..Default::default()
    }
}

/// The task side panel, next to the columns.
pub fn task_panel(theme: &Theme) -> container::Style {
    let p = ext(theme);
    container::Style {
        background: Some(p.background.base.color.into()),
        border: Border { color: p.background.strong.color.scale_alpha(0.5), width: 1.0, radius: 0.0.into() },
        text_color: Some(p.background.base.text),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_are_dark_and_have_surface() {
        let tn = tokyo_night();
        assert!(ext(&tn).is_dark);
        assert_eq!(tn.palette().background, rgb(0x1a1b26));
        assert_eq!(ext(&tn).background.weak.color, rgb(0x24283b));

        let drac = dracula();
        assert!(ext(&drac).is_dark);
        assert_eq!(drac.palette().background, rgb(0x282a36));
        assert_eq!(ext(&drac).background.weak.color, rgb(0x44475a));
    }

    #[test]
    fn violet_palette_and_surfaces() {
        let v = violet();
        assert!(ext(&v).is_dark);
        assert_eq!(v.palette().background, rgb(0x111319));
        assert_eq!(v.palette().text, rgb(0xe2e2eb));
        assert_eq!(v.palette().primary, rgb(0x7c3aed));
        assert_eq!(v.palette().success, rgb(0x4edea3));
        assert_eq!(v.palette().warning, rgb(0xe0a63a));
        assert_eq!(v.palette().danger, rgb(0xffb4ab));
        assert_eq!(ext(&v).background.weak.color, rgb(0x1e1f26));
        assert_eq!(ext(&v).background.weak.text, rgb(0xe2e2eb));
        assert_eq!(ext(&v).background.strong.color, rgb(0x33343b));
        assert_eq!(ext(&v).background.strong.text, rgb(0xe2e2eb));
    }

    #[test]
    fn header_and_selector_styles_follow_palettes() {
        for theme in [dark(), light(), tokyo_night(), dracula(), violet()] {
            let hdr = app_header(&theme);
            assert!(hdr.background.is_some(), "app_header must have a background in all themes");

            let pill = summary_pill(&theme);
            assert!(pill.background.is_some(), "summary_pill must have a background in all themes");

            let sel = workspace_selector(&theme, pick_list::Status::Active);
            assert_eq!(sel.border.radius, border::Radius::from(RADIUS_MD));

            let sel_hover = workspace_selector(&theme, pick_list::Status::Hovered);
            assert_ne!(sel.background, sel_hover.background);

            let menu = workspace_menu(&theme);
            assert_eq!(menu.border.radius, border::Radius::from(RADIUS_MD));

            let chat_hdr = header(&theme);
            assert!(chat_hdr.background.is_some(), "header must have a background in all themes");

            let seg_c = segmented_container(&theme);
            assert!(seg_c.background.is_some(), "segmented_container must have a background in all themes");
            assert_eq!(seg_c.border.radius, border::Radius::from(RADIUS_MD));

            let active_btn = segment_button(true)(&theme, button::Status::Active);
            assert!(active_btn.background.is_some(), "active segment button must have background");

            let inactive_btn = segment_button(false)(&theme, button::Status::Active);
            assert!(inactive_btn.background.is_none(), "inactive segment button must have transparent background");
        }

        // Check violet-specific tokens on header and segmented control
        let v = violet();
        assert_eq!(header(&v).background, Some(tokens::SURFACE_LOW.into()));
        assert_eq!(segmented_container(&v).background, Some(tokens::SURFACE_CARD.into()));
        let v_active = segment_button(true)(&v, button::Status::Active);
        assert_eq!(v_active.background, Some(v.palette().primary.into()));
        assert_eq!(v_active.text_color, tokens::ON_PRIMARY);
        let v_inactive = segment_button(false)(&v, button::Status::Active);
        assert_eq!(v_inactive.text_color, tokens::TEXT_MUTED);
    }
}

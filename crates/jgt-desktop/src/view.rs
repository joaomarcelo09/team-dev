//! Everything on screen: the session sidebar, the chat, the composer.

use crate::{
    app::{AgentRow, App, Conn, GitSwitch, MESSAGES_ID, Msg, ThemePref, ThemeStyle, Thread},
    icons, style,
};

// used from step 4 on
#[allow(unused_imports)]
pub use crate::app::{Module, Workspace, coordinator_for, partition_team_workers, workers_for, workspaces};
use std::collections::{HashMap, HashSet};
use chrono::{Datelike, Local, TimeZone};
use iced::{
    Alignment, Color, Element, Fill, Font, Padding, Shrink, Theme,
    font::Weight,
    widget::{
        Space, button, column, container, markdown, mouse_area, pick_list, row, rule, scrollable, space, stack, text, text_editor,
        text_input, tooltip,
    },
};
use jgt_core::{
    Delivery, Direction, Harness, Message, MessageKind, NotifyLevel, QuestionStatus, ReconnectMode, Session,
    SessionStatus,
};

pub(crate) fn bold() -> Font {
    Font { weight: Weight::Semibold, ..style::ui_font() }
}

pub(crate) fn muted<'a>(content: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(content).style(|theme: &Theme| text::Style { color: Some(style::muted(theme)) })
}

fn local(ms: i64) -> Option<chrono::DateTime<Local>> {
    Local.timestamp_millis_opt(ms).single()
}

pub(crate) fn clock(ms: i64) -> String {
    local(ms).map(|t| t.format("%H:%M").to_string()).unwrap_or_default()
}

pub(crate) fn short_time(ms: i64) -> String {
    let (Some(t), now) = (local(ms), Local::now()) else { return String::new() };
    if t.date_naive() == now.date_naive() {
        t.format("%H:%M").to_string()
    } else if t.year() == now.year() {
        t.format("%b %d").to_string()
    } else {
        t.format("%Y-%m-%d").to_string()
    }
}

fn day_label(ms: i64) -> String {
    let (Some(t), now) = (local(ms), Local::now()) else { return String::new() };
    let days = (now.date_naive() - t.date_naive()).num_days();
    match days {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        _ if t.year() == now.year() => t.format("%A, %B %d").to_string(),
        _ => t.format("%B %d, %Y").to_string(),
    }
}

fn status_label(s: &Session) -> &'static str {
    match s.status {
        SessionStatus::Active => "working",
        SessionStatus::Idle => "idle",
        SessionStatus::Listening => "listening",
        SessionStatus::Disconnected => "offline",
        SessionStatus::Ended => "ended",
    }
}

pub(crate) fn dot<'a>(color: Color, size: f32) -> Element<'a, Msg> {
    container(Space::new().width(size).height(size)).style(style::dot(color)).into()
}

pub(crate) fn tip<'a>(content: impl Into<Element<'a, Msg>>, label: &'a str) -> Element<'a, Msg> {
    tooltip(
        content,
        container(text(label).size(style::fs(12))).padding([6, 10]).style(container::rounded_box),
        tooltip::Position::Bottom,
    )
    .gap(4)
    .into()
}

/// An item in the workspace selector dropdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceChoice {
    Team { key: String, name: String },
    NewTeam,
}

impl std::fmt::Display for WorkspaceChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Team { name, .. } => write!(f, "{name}"),
            Self::NewTeam => write!(f, "New team…"),
        }
    }
}

/// Pure helper computing the active worker count and total worker cost for a workspace.
///
/// Matches the calculation used in the team panel:
/// - Working sessions are connected non-coordinator workers belonging to the workspace's coordinator.
/// - Cost is the sum of `cost_usd` across all workers of that coordinator (including archived and ended workers).
pub fn team_summary(sessions: &[Session], workspace: &str) -> (usize, f64) {
    let ws_norm = jgt_core::team::project_folder(workspace);
    if ws_norm.is_empty() {
        return (0, 0.0);
    }
    let Some(coord) = coordinator_for(sessions, ws_norm) else {
        return (0, 0.0);
    };
    let workers: Vec<&Session> = sessions
        .iter()
        .filter(|w| !w.is_coordinator() && w.parent_id.as_deref() == Some(coord.id.as_str()))
        .collect();
    let working = workers.iter().filter(|w| w.status.is_connected()).count();
    let cost: f64 = workers.iter().filter_map(|w| w.cost_usd).sum();
    (working, cost)
}

/// The top-level 56px application header.
pub fn app_header<'a>(app: &'a App) -> Element<'a, Msg> {
    let theme = app.theme();
    let p = theme.extended_palette();

    let brand = row![
        dot(p.primary.base.color, 8.0),
        text(jgt_core::APP_NAME)
            .size(style::fs(15))
            .font(bold())
            .style(|theme: &Theme| text::Style {
                color: Some(theme.extended_palette().primary.base.color),
            }),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let team_list = workspaces(&app.sessions);
    let mut choices: Vec<WorkspaceChoice> = team_list
        .into_iter()
        .map(|w| WorkspaceChoice::Team { key: w.key, name: w.name })
        .collect();
    if let Some(key) = app.workspace.as_deref()
        && !choices.iter().any(|c| matches!(c, WorkspaceChoice::Team { key: k, .. } if k == key))
    {
        let name = std::path::Path::new(key)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| key.to_string());
        choices.push(WorkspaceChoice::Team { key: key.to_string(), name });
    }
    choices.push(WorkspaceChoice::NewTeam);

    let selected: Option<WorkspaceChoice> = app.workspace.as_deref().and_then(|key| {
        choices.iter().find(|c| match c {
            WorkspaceChoice::Team { key: k, .. } => k == key,
            WorkspaceChoice::NewTeam => false,
        }).cloned()
    });

    let handle = iced::widget::pick_list::Handle::Static(iced::widget::pick_list::Icon {
        font: crate::icons::ICON_FONT,
        code_point: crate::icons::Icon::ExpandMore.codepoint(),
        size: Some(iced::Pixels(style::fs(14))),
        line_height: Default::default(),
        shaping: Default::default(),
    });

    let selector = pick_list(
        choices,
        selected.clone(),
        move |choice| match choice {
            WorkspaceChoice::Team { key, .. } => Msg::SelectWorkspace(key),
            WorkspaceChoice::NewTeam => Msg::NewTeamOpen,
        },
    )
    .placeholder("No team")
    .handle(handle)
    .text_size(style::fs(13))
    .padding([4, 10])
    .style(style::workspace_selector)
    .menu_style(style::workspace_menu);

    let folder_icon = icons::icon(icons::Icon::Folder, style::fs(14)).style(|theme: &Theme| text::Style {
        color: Some(theme.extended_palette().primary.base.color),
    });

    let mut ws_selector = row![folder_icon, selector].spacing(6).align_y(Alignment::Center);

    if let Some(WorkspaceChoice::Team { name, .. }) = &selected {
        let delete_team: Element<'a, Msg> = if app.confirm_delete_team {
            row![
                muted(format!("Delete team “{name}” and all its chats and tasks?")).size(style::fs(13)),
                button(text("Delete").size(style::fs(13)))
                    .on_press(Msg::DeleteTeamConfirm)
                    .style(style::danger_button)
                    .padding([6, 12]),
                button(text("Cancel").size(style::fs(13)))
                    .on_press(Msg::DeleteTeamCancel)
                    .style(style::ghost)
                    .padding([6, 10]),
            ]
            .spacing(6)
            .align_y(Alignment::Center)
            .into()
        } else {
            button(text("Delete team").size(style::fs(13)))
                .on_press(Msg::DeleteTeamAsk)
                .style(style::ghost)
                .padding([6, 10])
                .into()
        };
        ws_selector = ws_selector.push(delete_team);
    }

    let (working, cost) = app
        .workspace
        .as_deref()
        .map(|ws| team_summary(&app.sessions, ws))
        .unwrap_or((0, 0.0));

    let summary_text = if cost > 0.0 {
        format!("{working} working · ${cost:.2}")
    } else {
        format!("{working} working")
    };

    let dot_color = if working > 0 {
        p.success.base.color
    } else {
        style::muted(&theme)
    };

    let summary_pill = container(
        row![
            dot(dot_color, 6.0),
            text(summary_text).size(style::fs(12)),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .padding([4, 10])
    .style(style::summary_pill);

    let archive_btn = tip(
        button(icons::icon(icons::Icon::Archive, style::fs(16)))
            .on_press(Msg::ToggleArchived)
            .style(if app.show_archived { style::pill_button } else { style::ghost })
            .padding([6, 8]),
        if app.show_archived { "Hide archived chats" } else { "Show archived chats" },
    );

    let settings_btn = tip(
        button(icons::icon(icons::Icon::Settings, style::fs(16)))
            .on_press(Msg::ToggleSettings)
            .style(if app.show_settings { style::pill_button } else { style::ghost })
            .padding([6, 8]),
        "Theme, zoom and text size",
    );

    let bar = row![
        brand,
        Space::new().width(12),
        ws_selector,
        space::horizontal(),
        summary_pill,
        archive_btn,
        settings_btn,
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    container(bar)
        .width(Fill)
        .height(56)
        .padding([0, 16])
        .align_y(Alignment::Center)
        .style(style::app_header)
        .into()
}

pub fn view(app: &App) -> Element<'_, Msg> {
    let theme = app.theme();
    let body = row![
        bar_strip(Side::Left, app.left_collapsed),
        keep_mounted(sidebar(app, &theme), !app.left_collapsed),
        rule::vertical(1).style(rule::weak),
        main(app, &theme)
    ]
    .height(Fill);
    let layout = column![app_header(app), rule::horizontal(1).style(rule::weak), body]
        .width(Fill)
        .height(Fill);
    let mut root = stack![container(layout).width(Fill).height(Fill)];

    if app.show_settings {
        root = root.push(settings(app));
    }

    if let Some(toast) = &app.toast {
        let p = theme.extended_palette();
        let accent = if toast.error { p.danger.base.color } else { p.success.base.color };
        let card = container(
            row![
                text(toast.text.clone()).size(style::fs(13)),
                button(text("x").size(style::fs(13))).on_press(Msg::DismissToast).style(style::ghost).padding([0, 6])
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        )
        .padding([10, 14])
        .max_width(460)
        .style(style::banner(accent));
        root = root.push(
            container(card)
                .padding(18)
                .width(Fill)
                .height(Fill)
                .align_right(Fill)
                .align_bottom(Fill),
        );
    }

    root.into()
}

// ── settings ────────────────────────────────────────────────────────────────

fn settings(app: &App) -> Element<'_, Msg> {
    let stepper = |value: String, down: Msg, up: Msg, reset: Msg| {
        row![
            button(text("−").size(style::fs(14))).on_press(down).style(style::pill_button).padding([2, 12]),
            container(text(value).size(style::fs(14)).font(bold())).center_x(style::fs(14) * 4.0),
            button(text("+").size(style::fs(14))).on_press(up).style(style::pill_button).padding([2, 12]),
            button(text("Reset").size(style::fs(12))).on_press(reset).style(style::ghost).padding([2, 8]),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
    };
    let theme_button = |label: &'static str, pref: ThemePref| {
        button(text(label).size(style::fs(13)))
            .on_press(Msg::SetTheme(pref))
            .style(if app.theme_pref == pref { style::primary_button } else { style::pill_button })
            .padding([4, 12])
    };
    let style_button = |label: &'static str, s: ThemeStyle| {
        button(text(label).size(style::fs(13)))
            .on_press(Msg::SetStyle(s))
            .style(if app.theme_style == s { style::primary_button } else { style::pill_button })
            .padding([4, 12])
    };
    let line = |label: &'static str, control: Element<'static, Msg>| {
        row![text(label).size(style::fs(14)).width(style::fs(14) * 10.0), control].spacing(12).align_y(Alignment::Center)
    };
    let card = container(
        column![
            row![
                text("Settings").size(style::fs(18)).font(bold()),
                space::horizontal(),
                button(text("Close").size(style::fs(13))).on_press(Msg::ToggleSettings).style(style::ghost).padding([4, 8]),
            ]
            .align_y(Alignment::Center),
            scrollable(
                container(
                    column![
                        line(
                            "Theme",
                            row![
                                theme_button("Auto", ThemePref::System),
                                theme_button("Dark", ThemePref::Dark),
                                theme_button("Light", ThemePref::Light)
                            ]
                            .spacing(6)
                            .into()
                        ),
                        line(
                            "Pop-ups",
                            button(text(if app.popups { "On" } else { "Off" }).size(style::fs(13)))
                                .on_press(Msg::TogglePopups)
                                .style(if app.popups { style::primary_button } else { style::pill_button })
                                .padding([4, 12])
                                .into(),
                        ),
                        muted("Turns off only the desktop pop-up. The tray dot and unread counts stay.")
                            .size(style::fs(12)),
                        line(
                            "Auto-commit",
                            button(text(if app.auto_commit { "On" } else { "Off" }).size(style::fs(13)))
                                .on_press(Msg::ToggleGit(GitSwitch::Commit))
                                .style(if app.auto_commit { style::primary_button } else { style::pill_button })
                                .padding([4, 12])
                                .into(),
                        ),
                        line(
                            "Branch name",
                            text_input("jgt/{key}", &app.branch_pattern)
                                .on_input(Msg::GitBranchPattern)
                                .size(style::fs(13))
                                .padding([4, 8])
                                .style(style::input)
                                .width(220)
                                .into(),
                        ),
                        muted("{key} {slug} {title}, empty = jgt/<key>").size(style::fs(12)),
                        line(
                            "Auto-push",
                            button(text(if app.auto_push { "On" } else { "Off" }).size(style::fs(13)))
                                .on_press(Msg::ToggleGit(GitSwitch::Push))
                                .style(if app.auto_push { style::primary_button } else { style::pill_button })
                                .padding([4, 12])
                                .into(),
                        ),
                        line(
                            "Auto-open PR",
                            button(text(if app.auto_open_pr { "On" } else { "Off" }).size(style::fs(13)))
                                .on_press(Msg::ToggleGit(GitSwitch::OpenPr))
                                .style(if app.auto_open_pr { style::primary_button } else { style::pill_button })
                                .padding([4, 12])
                                .into(),
                        ),
                        line(
                            "Auto-merge PR",
                            button(text(if app.auto_merge_pr { "On" } else { "Off" }).size(style::fs(13)))
                                .on_press(Msg::ToggleGit(GitSwitch::MergePr))
                                .style(if app.auto_merge_pr { style::primary_button } else { style::pill_button })
                                .padding([4, 12])
                                .into(),
                        ),
                        muted("ON = the team does it after review. Never on main, never force-push.")
                            .size(style::fs(12)),
                        line(
                            "Style",
                            row![
                                style_button("JG Violet", ThemeStyle::Violet),
                                style_button("Default", ThemeStyle::Default),
                                style_button("Tokyo Night", ThemeStyle::TokyoNight),
                                style_button("Dracula", ThemeStyle::Dracula),
                            ]
                            .spacing(6)
                            .into()
                        ),
                        muted("JG Violet, Tokyo Night and Dracula are dark palettes only; Auto/Dark/Light applies to Default.")
                            .size(style::fs(12)),
                        line(
                            "Zoom",
                            stepper(
                                format!("{:.0}%", app.zoom * 100.0),
                                Msg::Zoom(crate::app::Step::Down),
                                Msg::Zoom(crate::app::Step::Up),
                                Msg::Zoom(crate::app::Step::Reset)
                            )
                            .into()
                        ),
                        line(
                            "Text size",
                            stepper(
                                app.text_size.to_string(),
                                Msg::TextSize(crate::app::Step::Down),
                                Msg::TextSize(crate::app::Step::Up),
                                Msg::TextSize(crate::app::Step::Reset)
                            )
                            .into()
                        ),
                        muted("Zoom scales the whole window (Ctrl + / Ctrl − / Ctrl 0). Text size changes only the letters.")
                            .size(style::fs(12)),
                        rule::horizontal(1).style(rule::weak),
                        agents_section(app),
                        config_banner(app),
                    ]
                    .spacing(16),
                )
                .padding(Padding { top: 0.0, right: 8.0, bottom: 0.0, left: 0.0 }),
            )
            .direction(scrollable::Direction::Vertical(scrollable::Scrollbar::default()))
            .width(Fill),
        ]
        .spacing(16),
    )
    .padding(22)
    .max_width(style::fs(14) * 46.0)
    .max_height(style::fs(14) * 42.0)
    .style(style::card(Color::TRANSPARENT));
    let backdrop = iced::widget::mouse_area(
        container(Space::new()).width(Fill).height(Fill).style(|_: &Theme| container::Style {
            background: Some(Color { a: 0.45, ..Color::BLACK }.into()),
            ..Default::default()
        }),
    )
    .on_press(Msg::ToggleSettings);
    stack![backdrop, container(iced::widget::opaque(card)).center(Fill).padding(16)].into()
}

/// The `[team]` config's warnings (unknown harness, effort the harness lacks, …): the team still
/// runs, on the replacement each warning names.
fn config_banner(app: &App) -> Element<'_, Msg> {
    if app.config_warnings.is_empty() {
        return Space::new().into();
    }
    let mut col = column![text("Team config").size(style::fs(13)).font(bold())].spacing(4);
    for w in &app.config_warnings {
        col = col.push(text(format!("⚠ {w}")).size(style::fs(12)));
    }
    container(col).padding([8, 12]).width(Fill).style(style::banner(Color::from_rgb(0.85, 0.55, 0.1))).into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessChoice(pub Harness);

impl std::fmt::Display for HarnessChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.label())
    }
}

pub const AGENT_ROLES: &[&str] = &[
    jgt_core::COORDINATOR,
    "coder",
    "reviewer",
    "git-coder",
];

pub(crate) fn agents_section(app: &App) -> Element<'_, Msg> {
    let role_width = style::fs(14) * 7.0;
    let harness_width = style::fs(14) * 9.5;
    let model_width = style::fs(14) * 8.0;
    let effort_width = style::fs(14) * 5.5;
    let suggestions_width = style::fs(14) * 8.5;

    let is_locked = app.agents_locked.is_some();
    let header_desc = if is_locked {
        "Changes apply to new workers on this team."
    } else {
        "Applies to teams started from now on."
    };

    let mut section = column![
        text("Agents").size(style::fs(16)).font(bold()),
        muted(header_desc).size(style::fs(12)),
    ]
    .spacing(10);

    let col_header = row![
        muted("ROLE").size(style::fs(11)).font(bold()).width(role_width),
        muted("HARNESS").size(style::fs(11)).font(bold()).width(harness_width),
        muted("MODEL").size(style::fs(11)).font(bold()).width(model_width),
        muted("EFFORT").size(style::fs(11)).font(bold()).width(effort_width),
        muted("SUGGESTIONS").size(style::fs(11)).font(bold()).width(suggestions_width),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    section = section.push(col_header);

    for &role in AGENT_ROLES {
        let is_coordinator = role == jgt_core::COORDINATOR;
        let default_row;
        let agent = if let Some(a) = app.agents.iter().find(|a| a.role == role) {
            a
        } else if let Some(locked) = &app.agents_locked {
            if let Some(setting) = locked.roles.get(role) {
                default_row = AgentRow {
                    role: role.to_string(),
                    harness: setting.harness,
                    model: setting.model.clone(),
                    effort: setting.effort.clone(),
                };
                &default_row
            } else {
                default_row = AgentRow {
                    role: role.to_string(),
                    harness: Harness::ClaudeCode,
                    model: "opus".to_string(),
                    effort: "medium".to_string(),
                };
                &default_row
            }
        } else {
            default_row = AgentRow {
                role: role.to_string(),
                harness: Harness::ClaudeCode,
                model: "opus".to_string(),
                effort: "medium".to_string(),
            };
            &default_row
        };

        // Harness control
        let harness_control: Element<'_, Msg> = if is_coordinator {
            container(text("Claude Code").size(style::fs(13)))
                .width(harness_width)
                .align_y(Alignment::Center)
                .into()
        } else {
            let mut harness_options = app.installed_harnesses.clone();
            if !harness_options.contains(&agent.harness) {
                harness_options.push(agent.harness);
            }
            let choices: Vec<HarnessChoice> = harness_options.into_iter().map(HarnessChoice).collect();
            let selected = Some(HarnessChoice(agent.harness));
            let r_name = role.to_string();
            pick_list(choices, selected, move |choice| {
                Msg::SetAgentHarness(r_name.clone(), choice.0)
            })
            .text_size(style::fs(13))
            .padding([4, 8])
            .width(harness_width)
            .into()
        };

        // Model text input
        let r_name = role.to_string();
        let model_input = text_input("Model…", &agent.model)
            .on_input(move |m| Msg::SetAgentModel(r_name.clone(), m))
            .size(style::fs(13))
            .padding([4, 8])
            .style(style::input)
            .width(model_width);

        // Effort control
        let levels = jgt_core::harness::effort_levels(agent.harness);
        let r_name = role.to_string();
        let effort_control: Element<'_, Msg> = if levels.is_empty() {
            container(muted("n/a").size(style::fs(12)))
                .width(effort_width)
                .align_y(Alignment::Center)
                .into()
        } else {
            let selected = levels.iter().copied().find(|&l| l == agent.effort.as_str());
            pick_list(levels, selected, move |e| {
                Msg::SetAgentEffort(r_name.clone(), e.to_string())
            })
            .text_size(style::fs(13))
            .padding([4, 8])
            .width(effort_width)
            .into()
        };

        // Model suggestions control
        let r_name = role.to_string();
        let suggestions_control: Element<'_, Msg> = match app.harness_models.get(&agent.harness) {
            None => container(muted("Loading models…").size(style::fs(12)))
                .width(suggestions_width)
                .align_y(Alignment::Center)
                .into(),
            Some(Err(err)) => container(
                muted(format!("Couldn't list models ({err}); type a name"))
                    .size(style::fs(11))
                    .wrapping(text::Wrapping::Word),
            )
            .width(suggestions_width)
            .align_y(Alignment::Center)
            .into(),
            Some(Ok(suggestions)) if suggestions.is_empty() => container(
                muted("No suggestions").size(style::fs(12)),
            )
            .width(suggestions_width)
            .align_y(Alignment::Center)
            .into(),
            Some(Ok(suggestions)) => {
                let selected = suggestions.iter().find(|s| *s == &agent.model).cloned();
                pick_list(suggestions.as_slice(), selected, move |m| {
                    Msg::SetAgentModel(r_name.clone(), m)
                })
                .placeholder("Suggestions…")
                .text_size(style::fs(13))
                .padding([4, 8])
                .width(suggestions_width)
                .into()
            }
        };

        let r = row![
            text(role).size(style::fs(13)).font(bold()).width(role_width),
            harness_control,
            model_input,
            effort_control,
            suggestions_control,
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        section = section.push(r);

        if is_coordinator {
            let note = if is_locked {
                "The Coordinator always runs on Claude Code (only Claude can be the coordinator). Applies after a reset."
            } else {
                "The Coordinator always runs on Claude Code (only Claude can be the coordinator)."
            };
            section = section.push(muted(note).size(style::fs(12)));
        }
    }

    // Action button
    let action_row = if is_locked {
        row![
            button(text("Apply to this team").size(style::fs(13)))
                .on_press(Msg::ApplyAgents)
                .style(style::primary_button)
                .padding([5, 18]),
            muted("New workers use these settings.").size(style::fs(12)),
        ]
        .spacing(12)
        .align_y(Alignment::Center)
    } else {
        row![
            button(text("Save").size(style::fs(13)))
                .on_press(Msg::SaveAgents)
                .style(style::primary_button)
                .padding([5, 18]),
            muted("Applies to teams started from now on.").size(style::fs(12)),
        ]
        .spacing(12)
        .align_y(Alignment::Center)
    };

    section = section.push(action_row);

    section.into()
}

// ── sidebar ─────────────────────────────────────────────────────────────────

/// The data model for a workspace's sidebar rows: Board task count, Coordinator, and workers.
#[derive(Debug, Clone, PartialEq)]
pub struct SidebarWorkspaceModel<'a> {
    pub workspace: &'a str,
    pub board_open_tasks: Option<usize>,
    pub coordinator: Option<&'a Session>,
    pub workers: Vec<&'a Session>,
    pub workers_collapsed: bool,
}

pub fn sidebar_workspace_model<'a>(
    sessions: &'a [Session],
    boards: &'a HashMap<String, crate::board::Board>,
    workspace: &'a str,
    workers_collapsed: bool,
) -> SidebarWorkspaceModel<'a> {
    let board_open_tasks = boards.get(workspace).map(|b| {
        b.tasks().iter().filter(|t| t.status != jgt_core::TaskStatus::Done).count()
    });
    let coordinator = coordinator_for(sessions, workspace);
    let workers = workers_for(sessions, workspace);
    SidebarWorkspaceModel {
        workspace,
        board_open_tasks,
        coordinator,
        workers,
        workers_collapsed,
    }
}

/// Which side bar a toggle belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// The sidebar's text inputs, unfocused when the bar collapses so keys do not go to a hidden field.
pub const SIDEBAR_SEARCH_ID: &str = "sidebar-search";
pub const NEW_TEAM_FOLDER_ID: &str = "new-team-folder";

/// Width of a collapsed side bar: a strip holding only the expand chevron.
pub const COLLAPSED_BAR_WIDTH: f32 = 28.0;

/// The chevron button that collapses (`collapsed == false`) or expands a side bar. The chevron
/// points the way the bar moves: a left bar collapses leftwards, a right bar rightwards.
pub fn bar_toggle<'a>(side: Side, collapsed: bool) -> Element<'a, Msg> {
    let points_left = (side == Side::Left) != collapsed;
    let (msg, label) = match (side, collapsed) {
        (Side::Left, false) => (Msg::ToggleLeftBar, "Collapse side bar (Ctrl+B)"),
        (Side::Left, true) => (Msg::ToggleLeftBar, "Expand side bar (Ctrl+B)"),
        (Side::Right, false) => (Msg::ToggleRightBar, "Collapse file picker (Ctrl+Alt+B)"),
        (Side::Right, true) => (Msg::ToggleRightBar, "Expand file picker (Ctrl+Alt+B)"),
    };
    tip(
        button(text(if points_left { "‹" } else { "›" }).size(style::fs(16)))
            .on_press(msg)
            .style(style::ghost)
            .padding([0, 6]),
        label,
    )
}

/// The strip shown in place of a collapsed side bar, or nothing while the bar is expanded.
/// It sits next to the bar (see [`keep_mounted`]) so the row's children never change.
pub fn bar_strip<'a>(side: Side, collapsed: bool) -> Element<'a, Msg> {
    if collapsed { collapsed_bar(side) } else { Space::new().width(0).height(0).into() }
}

/// Keeps a side bar in the widget tree while it is collapsed, at zero width, so its widget
/// state (scroll positions above all) is still there when it is expanded again.
pub fn keep_mounted<'a>(bar: Element<'a, Msg>, visible: bool) -> Element<'a, Msg> {
    let wrapper = container(bar).height(Fill).clip(true);
    if visible { wrapper.into() } else { wrapper.width(0).into() }
}

/// A collapsed side bar: a thin strip with only the expand chevron.
/// The strip keeps its bar's look: the left one the sidebar background, the right one (the
/// IDE picker) the plain view background.
pub fn collapsed_bar<'a>(side: Side) -> Element<'a, Msg> {
    let strip = container(bar_toggle(side, true))
        .width(COLLAPSED_BAR_WIDTH)
        .height(Fill)
        .padding([8, 0])
        .align_x(Alignment::Center);
    match side {
        Side::Left => strip.style(style::sidebar).into(),
        Side::Right => strip.into(),
    }
}

fn sidebar<'a>(app: &'a App, theme: &Theme) -> Element<'a, Msg> {
    let p = theme.extended_palette();

    let search = text_input("Search chats", &app.search)
        .id(SIDEBAR_SEARCH_ID)
        .on_input(Msg::Search)
        .padding([7, 10])
        .size(style::fs(13))
        .style(style::input);
    let search = row![search, bar_toggle(Side::Left, false)].spacing(4).align_y(Alignment::Center);

    let query = app.search.trim().to_lowercase();
    let searching = !query.is_empty();
    let showing_archived = app.show_archived;

    let mut list = column![].spacing(3);

    if searching || showing_archived {
        let matches = |s: &&Session| {
            if showing_archived && !searching && !s.archived {
                return false;
            }
            if !showing_archived && s.archived && !searching {
                return false;
            }
            query.is_empty()
                || s.display_name().to_lowercase().contains(&query)
                || s.cwd.as_deref().unwrap_or("").to_lowercase().contains(&query)
                || s.harness.label().to_lowercase().contains(&query)
                || s.role.as_deref().unwrap_or("").to_lowercase().contains(&query)
        };
        let visible: Vec<&Session> = app.sessions.iter().filter(matches).collect();

        #[allow(clippy::type_complexity)]
        let groups: [(&str, Box<dyn Fn(&Session) -> bool>); 4] = [
            ("ONLINE", Box::new(|s: &Session| !s.archived && s.status.is_connected())),
            ("OFFLINE", Box::new(|s: &Session| !s.archived && s.status == SessionStatus::Disconnected)),
            ("ENDED", Box::new(|s: &Session| !s.archived && s.status == SessionStatus::Ended)),
            ("ARCHIVED", Box::new(|s: &Session| s.archived)),
        ];
        let mut any = false;
        for (label, include) in groups.iter() {
            if *label == "ARCHIVED" && !showing_archived && !searching {
                continue;
            }
            let items = sidebar_items(&visible, include, &app.collapsed_coordinators);
            if items.is_empty() {
                continue;
            }
            any = true;
            list = list.push(
                container(muted(format!("{label} · {}", items.len())).size(style::fs(11)).font(bold())).padding([10, 8]),
            );
            for s in items {
                let collapsed = s.is_coordinator().then(|| app.is_coordinator_collapsed(&s.id));
                let item = session_item(s, app.selected.as_deref() == Some(s.id.as_str()), collapsed, theme);
                list = list.push(if s.is_worker() { container(item).padding(Padding::ZERO.left(16)).into() } else { item });
            }
        }
        if !any {
            list = list.push(container(muted("Nothing matches.").size(style::fs(13))).padding(12));
        }
    } else if let Some(ws) = &app.workspace {
        let model = sidebar_workspace_model(&app.sessions, &app.boards, ws, app.workers_collapsed);

        // 1. Board row
        let board_selected = app.module == Module::Board;
        let mut board_row = row![
            icons::icon(icons::Icon::Board, style::fs(16)),
            text("Board")
                .size(style::fs(13))
                .font(if board_selected { bold() } else { style::ui_font() }),
            space::horizontal(),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        if let Some(open_count) = model.board_open_tasks {
            board_row = board_row.push(
                container(
                    text(open_count.to_string())
                        .size(style::fs(10))
                        .font(bold())
                        .style(|theme: &Theme| text::Style {
                            color: Some(theme.extended_palette().primary.base.color),
                        }),
                )
                .padding([1, 6])
                .style(style::pill),
            );
        }

        let board_btn = button(board_row)
            .on_press(Msg::SelectModule(Module::Board))
            .style(style::session_item(board_selected))
            .width(Fill)
            .padding([7, 10]);

        list = list.push(board_btn);

        // IDE row
        let ide_selected = app.module == Module::Ide;
        let ide_row = row![
            icons::icon(icons::Icon::Folder, style::fs(16)),
            text("IDE")
                .size(style::fs(13))
                .font(if ide_selected { bold() } else { style::ui_font() }),
            space::horizontal(),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let ide_btn = button(ide_row)
            .on_press(Msg::SelectModule(Module::Ide))
            .style(style::session_item(ide_selected))
            .width(Fill)
            .padding([7, 10]);

        list = list.push(ide_btn);

        // 2. Coordinator row
        let coord_selected = app.module == Module::Coordinator;
        let mut coord_right = row![].spacing(6).align_y(Alignment::Center);
        if let Some(coord) = model.coordinator {
            coord_right = coord_right.push(dot(style::status_color(theme, coord.status), 7.0));
            if coord.unread > 0 {
                coord_right = coord_right.push(
                    container(
                        text(coord.unread.to_string())
                            .size(style::fs(10))
                            .font(bold()),
                    )
                    .padding([1, 6])
                    .style(style::badge(p.primary.base.color)),
                );
            }
        }

        let coord_row = row![
            icons::icon(icons::Icon::Coordinator, style::fs(16)),
            text("Coordinator")
                .size(style::fs(13))
                .font(if coord_selected { bold() } else { style::ui_font() }),
            space::horizontal(),
            coord_right,
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let coord_btn = button(coord_row)
            .on_press(Msg::SelectModule(Module::Coordinator))
            .style(style::session_item(coord_selected))
            .width(Fill)
            .padding([7, 10]);

        list = list.push(coord_btn);

        // 3. Worker section
        let chevron = if app.workers_collapsed {
            icons::Icon::ChevronRight
        } else {
            icons::Icon::ExpandMore
        };

        let worker_header_row = row![
            icons::icon(chevron, style::fs(14)),
            text("Worker").size(style::fs(12)).font(bold()),
            space::horizontal(),
            container(
                text(model.workers.len().to_string()).size(style::fs(10)),
            )
            .padding([1, 6])
            .style(style::pill),
        ]
        .spacing(6)
        .align_y(Alignment::Center);

        let worker_header_btn = button(worker_header_row)
            .on_press(Msg::ToggleWorkers)
            .style(style::ghost)
            .width(Fill)
            .padding([6, 8]);

        list = list.push(worker_header_btn);

        if !app.workers_collapsed {
            let now = jgt_core::now_ms();
            for w in &model.workers {
                let is_selected = app.module == Module::Worker(w.id.clone());
                let (state, secs) = match w.status {
                    SessionStatus::Active => ("working", (now - w.created_at) / 1000),
                    SessionStatus::Idle | SessionStatus::Listening => ("idle", (now - w.created_at) / 1000),
                    _ => (
                        if w.status == SessionStatus::Ended { "ended" } else { "done" },
                        w.run_secs.unwrap_or((w.updated_at - w.created_at) / 1000),
                    ),
                };
                let dur = duration(secs);
                let cost = match w.cost_usd {
                    Some(c) => format!("${c:.2}"),
                    None if w.status.is_connected() => String::new(),
                    None => "—".to_string(),
                };
                let name = match w.role.as_deref() {
                    Some(role) => role.to_string(),
                    None => w.display_name(),
                };

                let mut info = format!("{state} · {dur}");
                if !cost.is_empty() {
                    info.push_str(&format!(" · {cost}"));
                }

                let mut right = row![muted(info).size(style::fs(11)).wrapping(text::Wrapping::None)]
                    .spacing(6)
                    .align_y(Alignment::Center);

                if w.unread > 0 {
                    right = right.push(
                        container(text(w.unread.to_string()).size(style::fs(10)).font(bold()))
                            .padding([1, 5])
                            .style(style::badge(p.primary.base.color)),
                    );
                }

                let line = row![
                    dot(style::status_color(theme, w.status), 6.0),
                    text(name).size(style::fs(12)).wrapping(text::Wrapping::None).width(Fill),
                    right,
                ]
                .spacing(6)
                .align_y(Alignment::Center);

                let worker_btn = button(line)
                    .on_press(Msg::SelectModule(Module::Worker(w.id.clone())))
                    .style(style::session_item(is_selected))
                    .width(Fill)
                    .padding([5, 8]);

                list = list.push(container(worker_btn).padding(Padding::ZERO.left(8)));
            }
        }
    } else {
        let hint = if app.sessions.is_empty() {
            "No team sessions yet.\n\nClick New team and pick a project folder: its Coordinator starts and greets you here."
        } else {
            "Select a team from the header above."
        };
        let mut hint_col = column![muted(hint).size(style::fs(13))].spacing(8);
        hint_col = hint_col.push(
            button(text("New team").size(style::fs(13)).align_x(Alignment::Center))
                .on_press(Msg::NewTeamOpen)
                .style(style::primary_button)
                .padding([6, 12]),
        );
        list = list.push(container(hint_col).padding(12));
    }

    let mut sidebar_col = column![].spacing(8);
    if let Some(folder) = &app.new_team {
        let input = text_input("Project folder", folder)
            .id(NEW_TEAM_FOLDER_ID)
            .padding([7, 10])
            .size(style::fs(13))
            .style(style::input);
        let input = if app.starting_team {
            input
        } else {
            input.on_input(Msg::NewTeamInput).on_submit(Msg::NewTeamStart)
        };
        let start = button(text(if app.starting_team { "Starting…" } else { "Start" }).size(style::fs(12)))
            .style(style::primary_button)
            .padding([6, 10])
            .on_press_maybe((!app.starting_team).then_some(Msg::NewTeamStart));
        let cancel = button(text("Cancel").size(style::fs(12)))
            .style(style::ghost)
            .padding([6, 10])
            .on_press_maybe((!app.starting_team).then_some(Msg::NewTeamCancel));
        let browse = button(text("Choose folder…").size(style::fs(12)))
            .style(style::pill_button)
            .padding([6, 10])
            .on_press_maybe((!app.starting_team).then_some(Msg::NewTeamBrowse));
        let mut form = column![
            muted("New team session: the Coordinator works in this folder").size(style::fs(11)),
            input,
            row![browse, space::horizontal(), cancel, start].spacing(6),
        ]
        .spacing(6);
        let recent = app.recent_folders();
        if !recent.is_empty() {
            form = form.push(muted("Recent").size(style::fs(11)).font(bold()));
            let home = jgt_core::paths::home_dir().display().to_string();
            for folder in recent {
                let label = match folder.strip_prefix(&home) {
                    Some(rest) => format!("~{rest}"),
                    None => folder.to_string(),
                };
                let picked = folder == folder_value(app);
                form = form.push(
                    button(text(label).size(style::fs(12)).wrapping(text::Wrapping::None))
                        .on_press_maybe((!app.starting_team).then(|| Msg::NewTeamInput(folder.to_string())))
                        .style(style::session_item(picked))
                        .width(Fill)
                        .padding([4, 8]),
                );
            }
        }
        sidebar_col = sidebar_col.push(container(form).padding([0, 10]));
    }

    sidebar_col = sidebar_col
        .push(container(search).padding([0, 10]))
        .push(scrollable(container(list).padding([4, 6])).height(Fill));

    container(sidebar_col)
        .width(style::fs(14) * 288.0 / f32::from(jgt_core::config::Desktop::DEFAULT_TEXT_SIZE))
        .height(Fill)
        .style(style::sidebar)
        .into()
}

/// The folder typed or picked in the New team form.
fn folder_value(app: &App) -> &str {
    app.new_team.as_deref().unwrap_or("").trim_end_matches('/')
}

/// Which session items to display for a sidebar group.
///
/// Top-level sessions in `visible` matching `include` are listed. For any Coordinator (or parent session),
/// its workers are listed beneath it unless the Coordinator is collapsed.
pub fn sidebar_items<'a>(
    visible: &[&'a Session],
    include: impl Fn(&Session) -> bool,
    collapsed: &HashSet<String>,
) -> Vec<&'a Session> {
    let parent_listed = |w: &Session| visible.iter().any(|p| Some(p.id.as_str()) == w.parent_id.as_deref());
    let mut items: Vec<&'a Session> = Vec::new();
    for s in visible.iter().copied().filter(|s| include(s) && !(s.is_worker() && parent_listed(s))) {
        items.push(s);
        if !s.is_worker() && !collapsed.contains(&s.id) {
            items.extend(visible.iter().copied().filter(|w| w.is_worker() && w.parent_id.as_deref() == Some(s.id.as_str())));
        }
    }
    items
}

fn session_item<'a>(
    s: &'a Session,
    selected: bool,
    collapsed: Option<bool>,
    theme: &Theme,
) -> Element<'a, Msg> {
    let p = theme.extended_palette();
    let time = s.last_message_at.unwrap_or(s.updated_at);
    let role = match s.role.as_deref() {
        Some(jgt_core::COORDINATOR) => "Coordinator".to_string(),
        Some(skill) => format!("↳ {skill}"),
        None => s.harness.label().to_string(),
    };
    let mut meta = row![dot(style::status_color(theme, s.status), 7.0), muted(role).size(style::fs(11))]
        .spacing(6)
        .align_y(Alignment::Center);
    if let Some(project) = s.project().filter(|p| Some(p.as_str()) != s.title.as_deref()) {
        meta = meta.push(muted(format!("· {project}")).size(style::fs(11)));
    }
    let mut meta_row = row![meta, space::horizontal()].align_y(Alignment::Center);
    if s.queued > 0 {
        meta_row = meta_row.push(tip(muted(format!("{} queued", s.queued)).size(style::fs(11)), "Your messages waiting for the agent"));
    }
    if s.unread > 0 {
        meta_row = meta_row.push(
            container(text(s.unread.to_string()).size(style::fs(11)).font(bold()))
                .padding([1, 7])
                .style(style::badge(p.primary.base.color)),
        );
    }
    let preview = s.status_text.clone().or_else(|| s.last_message.clone()).unwrap_or_default();
    let name_font = if s.unread > 0 { bold() } else { style::ui_font() };
    let mut title_left = row![].spacing(4).align_y(Alignment::Center);
    if let Some(is_collapsed) = collapsed {
        let arrow = if is_collapsed { "▸" } else { "▾" };
        let tooltip_msg = if is_collapsed { "Expand workers" } else { "Collapse workers" };
        let arrow_btn = tip(
            button(text(arrow).size(style::fs(11)))
                .on_press(Msg::ToggleCoordinatorCollapse(s.id.clone()))
                .style(style::ghost)
                .padding([1, 4]),
            tooltip_msg,
        );
        title_left = title_left.push(arrow_btn);
    }
    title_left = title_left.push(text(s.display_name()).size(style::fs(14)).font(name_font).wrapping(text::Wrapping::None));
    let content = column![
        row![
            title_left,
            space::horizontal(),
            muted(short_time(time)).size(style::fs(11))
        ]
        .spacing(6)
        .align_y(Alignment::Center),
        meta_row.spacing(6),
        muted(preview).size(style::fs(12)).wrapping(text::Wrapping::None),
    ]
    .spacing(3);
    button(container(content).clip(true))
        .on_press(Msg::Select(s.id.clone()))
        .style(style::session_item(selected))
        .width(Fill)
        .padding([8, 10])
        .into()
}

// ── main area ───────────────────────────────────────────────────────────────

fn main<'a>(app: &'a App, theme: &Theme) -> Element<'a, Msg> {
    if app.module == Module::Board {
        if let Some(ws) = app.workspace.as_deref() {
            return crate::board::board(app, ws, theme);
        } else {
            return container(muted("No workspace selected.")).center(Fill).into();
        }
    }
    if app.module == Module::Ide {
        return crate::ide_view::ide(app);
    }
    let Some(session) = app.selected_session() else { return welcome(app) };
    let thread = app.threads.get(&session.id);
    column![
        chat_header(app, session, theme),
        rule::horizontal(1).style(rule::weak),
        messages(app, session, thread, theme),
        composer(app, session),
    ]
    .width(Fill)
    .height(Fill)
    .into()
}

/// The number of Coordinators that are not archived and not ended.
pub fn count_coordinators(sessions: &[Session]) -> usize {
    sessions
        .iter()
        .filter(|s| s.is_coordinator() && !s.archived && s.status != SessionStatus::Ended)
        .count()
}

fn welcome(app: &App) -> Element<'_, Msg> {
    let status = match &app.conn {
        Conn::Connected => {
            let coordinators = count_coordinators(&app.sessions);
            format!(
                "{} session{} · {} Coordinator{}",
                app.sessions.len(),
                if app.sessions.len() == 1 { "" } else { "s" },
                coordinators,
                if coordinators == 1 { "" } else { "s" },
            )
        }
        Conn::Connecting => "Connecting to the hub…".into(),
        Conn::Offline(_) => "The hub is not running. Start it with: systemctl --user enable --now jg-team-dev-hub".into(),
    };
    let steps = column![
        muted("1.  Click New team and pick a project folder").size(style::fs(14)),
        muted("2.  Its Coordinator starts and greets you here").size(style::fs(14)),
        muted("3.  Tell it what you need; it listens for 50 minutes").size(style::fs(14)),
    ]
    .spacing(6);
    container(
        column![
            text(jgt_core::APP_NAME).size(style::fs(34)).font(bold()),
            muted("Talk to a Coordinator; it runs your AI dev team.").size(style::fs(16)),
            Space::new().height(12),
            steps,
            Space::new().height(12),
            muted(status).size(style::fs(13)),
        ]
        .spacing(6)
        .align_x(Alignment::Center),
    )
    .center(Fill)
    .into()
}

fn chat_header<'a>(app: &'a App, s: &'a Session, theme: &Theme) -> Element<'a, Msg> {
    let title: Element<'a, Msg> = match &app.rename {
        Some(value) => row![
            text_input("Session title", value)
                .id("rename")
                .on_input(Msg::RenameInput)
                .on_submit(Msg::RenameSave)
                .size(style::fs(16))
                .padding([5, 8])
                .width(320)
                .style(style::input),
            button(text("Save").size(style::fs(13))).on_press(Msg::RenameSave).style(style::primary_button).padding([6, 12]),
            button(text("Cancel").size(style::fs(13))).on_press(Msg::RenameCancel).style(style::ghost).padding([6, 10]),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        .into(),
        None => tip(
            button(text(s.display_name()).size(style::fs(18)).font(bold())).on_press(Msg::RenameStart).style(style::ghost).padding([2, 6]),
            "Click to rename",
        ),
    };

    let mut subtitle = row![
        dot(style::status_color(theme, s.status), 8.0),
        text(status_label(s)).size(style::fs(12)),
        muted("·").size(style::fs(12)),
        muted(runs_on(s)).size(style::fs(12)),
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    if let Some(cwd) = &s.cwd {
        subtitle = subtitle
            .push(muted("·").size(style::fs(12)))
            .push(muted(cwd.as_str()).size(style::fs(12)).wrapping(text::Wrapping::None));
    }
    if let Some(status_text) = &s.status_text {
        subtitle = subtitle.push(
            container(text(status_text.as_str()).size(style::fs(11)).wrapping(text::Wrapping::None)).padding([2, 8]).style(style::pill),
        );
    }

    let listen_btn = tip(
        button(
            row![
                icons::icon(icons::Icon::Listen, style::fs(14)),
                text("Listen").size(style::fs(12)),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .on_press_maybe(listen_enabled(s).then_some(Msg::SetListen(!s.listen)))
        .style(style::segment_button(s.listen))
        .padding([4, 10]),
        "When its turn ends, the agent keeps waiting for your messages instead of going idle",
    );
    let replies_btn = tip(
        button(
            row![
                icons::icon(icons::Icon::Replies, style::fs(14)),
                text("Replies").size(style::fs(12)),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .on_press(Msg::SetMirror(!s.mirror))
        .style(style::segment_button(s.mirror))
        .padding([4, 10]),
        "Show every final reply here, not only replies to messages you sent from JG Team Dev",
    );
    let segment_control = container(
        row![listen_btn, replies_btn].spacing(2).align_y(Alignment::Center),
    )
    .padding(2)
    .style(style::segmented_container);

    let archive = button(text(if s.archived { "Unarchive" } else { "Archive" }).size(style::fs(13)))
        .on_press(Msg::SetArchived(!s.archived))
        .style(style::ghost)
        .padding([6, 10]);
    let delete: Element<'a, Msg> = if app.confirm_delete {
        row![
            muted("Delete this conversation?").size(style::fs(13)),
            button(text("Delete").size(style::fs(13))).on_press(Msg::DeleteConfirm).style(style::danger_button).padding([6, 12]),
            button(text("Cancel").size(style::fs(13))).on_press(Msg::DeleteCancel).style(style::ghost).padding([6, 10]),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        .into()
    } else {
        button(text("Delete").size(style::fs(13))).on_press(Msg::DeleteAsk).style(style::ghost).padding([6, 10]).into()
    };

    let controls = row![segment_control, archive, delete].spacing(12).align_y(Alignment::Center);
    // The title side takes what the controls leave and clips (long paths, large text).
    let top = row![column![title, subtitle].spacing(4).width(Fill).clip(true), controls]
        .spacing(16)
        .align_y(Alignment::Center);

    let mut col = column![top].spacing(10);
    if let Some(banner) = status_banner(app, s, theme) {
        col = col.push(banner);
    }
    container(col).padding([12, 18]).width(Fill).style(style::header).into()
}

/// Pure helper: returns true if the Listen toggle is enabled for the session.
///
/// Listen is disabled when the session has ended or uses the Custom harness.
pub fn listen_enabled(s: &Session) -> bool {
    s.status != SessionStatus::Ended && s.harness != Harness::Custom
}

/// What a session runs on: `coder · OpenCode · <model> · high` for a worker (PRD H7), the
/// harness alone otherwise. A worker whose harness has no effort setting shows "effort: n/a".
fn runs_on(s: &Session) -> String {
    let Some(role) = s.role.as_deref().filter(|_| s.is_worker()) else { return s.harness.label().to_string() };
    let mut parts = vec![role.to_string(), s.harness.label().to_string()];
    parts.extend(s.model.clone());
    parts.push(s.effort.clone().filter(|e| !e.is_empty()).unwrap_or_else(|| "effort: n/a".into()));
    parts.join(" · ")
}

/// `1m05s`, `2h03m`.
fn duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs >= 3600 { format!("{}h{:02}m", secs / 3600, secs % 3600 / 60) } else { format!("{}m{:02}s", secs / 60, secs % 60) }
}

fn status_banner<'a>(app: &'a App, s: &'a Session, theme: &Theme) -> Option<Element<'a, Msg>> {
    let p = theme.extended_palette();
    let (accent, message, actions): (Color, String, Option<Element<'a, Msg>>) = match s.status {
        SessionStatus::Disconnected => {
            let reason = s.ended_reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default();
            let queued = if s.queued > 0 {
                format!(" {} message{} waiting.", s.queued, if s.queued == 1 { "" } else { "s" })
            } else {
                String::new()
            };
            let actions = s.harness.can_reconnect().then(|| {
                let enabled = !app.reconnecting;
                row![
                    tip(
                        button(text(if app.reconnecting { "Starting…" } else { "Reconnect" }).size(style::fs(13)))
                            .on_press_maybe(enabled.then_some(Msg::Reconnect(ReconnectMode::Background)))
                            .style(style::primary_button)
                            .padding([6, 12]),
                        "Resume the session in the background; it stays reachable from JG Team Dev",
                    ),
                    tip(
                        button(text("Open in terminal").size(style::fs(13)))
                            .on_press_maybe(enabled.then_some(Msg::Reconnect(ReconnectMode::Terminal)))
                            .style(style::pill_button)
                            .padding([6, 12]),
                        "Resume the session in a new terminal window",
                    ),
                ]
                .spacing(8)
                .into()
            });
            (
                p.warning.base.color,
                format!("The agent is offline{reason}. Messages you send are kept and delivered when it reconnects.{queued}"),
                actions,
            )
        }
        SessionStatus::Ended => (
            style::muted(theme),
            format!(
                "This session ended ({}). Its context is gone, so the conversation is read-only.",
                s.ended_reason.as_deref().unwrap_or("closed")
            ),
            None,
        ),
        SessionStatus::Listening => {
            (p.success.base.color, "Listening — your messages go straight to the agent.".into(), None)
        }
        SessionStatus::Idle if s.queued > 0 && s.harness == Harness::ClaudeCode => (
            p.primary.base.color,
            "Idle — your messages are delivered when the agent's next turn starts. Turn on Listen to keep it waiting for you."
                .into(),
            None,
        ),
        _ => return None,
    };
    let mut content = row![text(message).size(style::fs(13)).width(Fill)].spacing(12).align_y(Alignment::Center);
    if let Some(actions) = actions {
        content = content.push(actions);
    }
    Some(container(content).padding([8, 12]).width(Fill).style(style::banner(accent)).into())
}

fn messages<'a>(app: &'a App, s: &'a Session, thread: Option<&'a Thread>, theme: &Theme) -> Element<'a, Msg> {
    let Some(thread) = thread.filter(|t| t.loaded) else {
        return container(muted("Loading…")).center(Fill).into();
    };
    let mut md_style = markdown::Style::from_palette(theme.palette());
    md_style.font = style::ui_font();
    let settings = markdown::Settings::with_text_size(style::fs(14), md_style);

    let mut col = column![].spacing(10).padding([16, 22]).width(Fill);
    if thread.has_more {
        col = col.push(
            container(button(text("Load older messages").size(style::fs(12))).on_press(Msg::LoadOlder).style(style::pill_button))
                .center_x(Fill),
        );
    }
    if thread.messages.is_empty() {
        col = col.push(container(muted("No messages yet.").size(style::fs(13))).center_x(Fill).padding(30));
    }
    let mut last_day = None;
    for m in &thread.messages {
        let day = local(m.created_at).map(|t| t.date_naive());
        if day != last_day {
            last_day = day;
            col = col.push(
                container(container(muted(day_label(m.created_at)).size(style::fs(11))).padding([2, 10]).style(style::pill))
                    .center_x(Fill)
                    .padding([6, 0]),
            );
        }
        col = col.push(message_view(app, s, thread, m, thread.markdown.get(&m.id), settings, theme));
    }
    scrollable(col).id(MESSAGES_ID).on_scroll(Msg::Scrolled).height(Fill).width(Fill).into()
}

#[derive(Debug, Clone, Copy)]
struct ChatViewer;

impl<'a> markdown::Viewer<'a, Msg> for ChatViewer {
    fn on_link_click(url: markdown::Uri) -> Msg {
        Msg::Link(url)
    }

    fn code_block(
        &self,
        settings: markdown::Settings,
        _language: Option<&'a str>,
        code: &'a str,
        lines: &'a [markdown::Text],
    ) -> Element<'a, Msg> {
        let copy_btn = tip(
            button(muted("Copy").size(style::fs(11)))
                .on_press(Msg::Copy(code.to_string()))
                .style(style::ghost)
                .padding([0, 6]),
            "Copy code block",
        );
        let bar = row![space::horizontal(), copy_btn].align_y(Alignment::Center);
        let block = markdown::code_block(settings, lines, Self::on_link_click);
        column![block, bar].spacing(2).into()
    }
}

#[allow(dead_code)]
pub fn normalize_indented_code_blocks(body: &str) -> String {
    use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};

    let mut replacements = Vec::new();
    let mut in_indented = false;
    let mut block_start = 0;
    let mut block_text = String::new();
    let mut container_depth: usize = 0;

    let parser = Parser::new(body).into_offset_iter();
    for (event, range) in parser {
        match event {
            Event::Start(Tag::BlockQuote(_) | Tag::List(_) | Tag::Item) => {
                container_depth += 1;
            }
            Event::End(TagEnd::BlockQuote(_) | TagEnd::List(_) | TagEnd::Item) => {
                container_depth = container_depth.saturating_sub(1);
            }
            Event::Start(Tag::CodeBlock(CodeBlockKind::Indented)) if container_depth == 0 => {
                in_indented = true;
                block_start = range.start;
                block_text.clear();
            }
            Event::Text(text) if in_indented => {
                block_text.push_str(&text);
            }
            Event::End(TagEnd::CodeBlock) if in_indented => {
                in_indented = false;
                replacements.push((block_start..range.end, block_text.clone()));
            }
            _ => {}
        }
    }

    if replacements.is_empty() {
        return body.to_string();
    }

    let mut out = String::new();
    let mut last_idx = 0;
    for (range, code) in replacements {
        out.push_str(&body[last_idx..range.start]);
        let needs_leading_newline = !out.is_empty() && !out.ends_with('\n');
        if needs_leading_newline {
            out.push('\n');
        }
        out.push_str("```\n");
        out.push_str(&code);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("```\n");
        last_idx = range.end;
    }
    out.push_str(&body[last_idx..]);
    out
}

pub fn parse_markdown(body: &str) -> Vec<markdown::Item> {
    markdown::parse(&normalize_indented_code_blocks(body)).collect()
}

#[allow(dead_code)]
pub fn extract_code_blocks(body: &str) -> Vec<String> {
    parse_markdown(body)
        .into_iter()
        .filter_map(|item| match item {
            markdown::Item::CodeBlock { code, .. } => Some(code),
            _ => None,
        })
        .collect()
}

fn md<'a>(items: Option<&'a Vec<markdown::Item>>, body: &'a str, settings: markdown::Settings) -> Element<'a, Msg> {
    match items {
        Some(items) => markdown::view_with(items, settings, &ChatViewer),
        None => text(body).size(style::fs(14)).into(),
    }
}

fn copy_button<'a>(body: &str) -> Element<'a, Msg> {
    tip(
        button(muted("Copy").size(style::fs(11))).on_press(Msg::Copy(body.to_string())).style(style::ghost).padding([0, 6]),
        "Copy the message text",
    )
}

fn copy_button_user<'a>(body: &str) -> Element<'a, Msg> {
    tip(
        button(
            text("Copy")
                .size(style::fs(11))
                .style(|theme: &Theme| text::Style { color: Some(theme.extended_palette().primary.base.text.scale_alpha(0.75)) }),
        )
        .on_press(Msg::Copy(body.to_string()))
        .style(style::ghost_on_primary)
        .padding([0, 6]),
        "Copy the message text",
    )
}

fn select_button<'a>(id: i64, selecting: bool) -> Element<'a, Msg> {
    if selecting {
        tip(
            button(text("Done").size(style::fs(11)).style(|theme: &Theme| text::Style {
                color: Some(theme.extended_palette().primary.base.color),
            }))
            .on_press(Msg::ToggleSelect(id))
            .style(style::ghost)
            .padding([0, 6]),
            "Done selecting text",
        )
    } else {
        tip(
            button(muted("Select").size(style::fs(11)))
                .on_press(Msg::ToggleSelect(id))
                .style(style::ghost)
                .padding([0, 6]),
            "Select text in this message",
        )
    }
}

fn select_button_user<'a>(id: i64, selecting: bool) -> Element<'a, Msg> {
    if selecting {
        tip(
            button(
                text("Done")
                    .size(style::fs(11))
                    .style(|theme: &Theme| text::Style { color: Some(theme.extended_palette().primary.base.text) }),
            )
            .on_press(Msg::ToggleSelect(id))
            .style(style::ghost_on_primary)
            .padding([0, 6]),
            "Done selecting text",
        )
    } else {
        tip(
            button(
                text("Select")
                    .size(style::fs(11))
                    .style(|theme: &Theme| text::Style { color: Some(theme.extended_palette().primary.base.text.scale_alpha(0.75)) }),
            )
            .on_press(Msg::ToggleSelect(id))
            .style(style::ghost_on_primary)
            .padding([0, 6]),
            "Select text in this message",
        )
    }
}

fn message_editor<'a>(editor: &'a text_editor::Content, id: i64, theme: &Theme) -> Element<'a, Msg> {
    let hl_theme = if theme.extended_palette().is_dark {
        iced::highlighter::Theme::Base16Mocha
    } else {
        iced::highlighter::Theme::InspiredGitHub
    };
    text_editor(editor)
        .highlight("md", hl_theme)
        .on_action(move |action| Msg::MessageEditor(id, action))
        .padding([8, 10])
        .size(style::fs(14))
        .font(style::ui_font())
        .wrapping(iced::widget::text::Wrapping::Word)
        .style(style::read_only_editor)
        .into()
}

fn message_user_editor<'a>(editor: &'a text_editor::Content, id: i64, theme: &Theme) -> Element<'a, Msg> {
    let hl_theme = if theme.extended_palette().is_dark {
        iced::highlighter::Theme::Base16Mocha
    } else {
        iced::highlighter::Theme::InspiredGitHub
    };
    text_editor(editor)
        .highlight("md", hl_theme)
        .on_action(move |action| Msg::MessageEditor(id, action))
        .padding([8, 10])
        .size(style::fs(14))
        .font(style::ui_font())
        .wrapping(iced::widget::text::Wrapping::Word)
        .style(style::read_only_user_editor)
        .into()
}

fn selectable_body<'a>(body: Element<'a, Msg>, id: i64) -> Element<'a, Msg> {
    mouse_area(body)
        .on_press(Msg::SelectMessage(id))
        .interaction(iced::mouse::Interaction::Text)
        .into()
}

fn message_view<'a>(
    app: &'a App,
    s: &'a Session,
    thread: &'a Thread,
    m: &'a Message,
    items: Option<&'a Vec<markdown::Item>>,
    settings: markdown::Settings,
    theme: &Theme,
) -> Element<'a, Msg> {
    let p = theme.extended_palette();
    let is_selecting = app.selecting_message == Some(m.id);
    let editor = thread.editors.get(&m.id);
    match (m.direction, m.kind) {
        (Direction::System, _) | (_, MessageKind::Event) => container(
            muted(format!("{} · {}", m.body, clock(m.created_at))).size(style::fs(12)),
        )
        .center_x(Fill)
        .into(),

        (Direction::ToAgent, _) => {
            let (status, failed) = match m.delivery {
                Delivery::Queued => ("queued", false),
                Delivery::Delivered | Delivery::Read => ("delivered", false),
                Delivery::Undeliverable => ("not delivered", true),
            };
            let time_status = text(format!("{} · {status}", clock(m.created_at))).size(style::fs(11)).style(move |theme: &Theme| {
                let p = theme.extended_palette();
                text::Style {
                    color: Some(if failed { p.danger.base.color } else { p.primary.base.text.scale_alpha(0.75) }),
                }
            });
            let footer = row![
                time_status,
                space::horizontal(),
                select_button_user(m.id, is_selecting),
                copy_button_user(&m.body),
            ]
            .spacing(6)
            .align_y(Alignment::Center);
            let content: Element<'a, Msg> = if is_selecting && let Some(ed) = editor {
                message_user_editor(ed, m.id, theme)
            } else {
                selectable_body(text(m.body.as_str()).size(style::fs(14)).into(), m.id)
            };
            let bubble = container(column![content, footer].spacing(4))
                .padding([8, 12])
                .max_width(560)
                .style(if failed { style::bubble_failed } else { style::bubble_user });
            row![space::horizontal(), bubble].into()
        }

        (Direction::FromAgent, MessageKind::Question | MessageKind::Permission) => {
            question_view(app, s, thread, m, items, settings, theme)
        }

        (Direction::FromAgent, MessageKind::Notify) => {
            let level = m.meta_str("level").and_then(|l| l.parse().ok()).unwrap_or(NotifyLevel::Info);
            let accent = style::level_color(theme, level);
            let footer = row![
                muted(format!("{} · notification", clock(m.created_at))).size(style::fs(11)),
                space::horizontal(),
                select_button(m.id, is_selecting),
                copy_button(&m.body),
            ]
            .spacing(6)
            .align_y(Alignment::Center);
            let content: Element<'a, Msg> = if is_selecting && let Some(ed) = editor {
                message_editor(ed, m.id, theme)
            } else {
                selectable_body(md(items, &m.body, settings), m.id)
            };
            let card = container(
                row![dot(accent, 10.0), column![content, footer].spacing(6).width(Fill)]
                    .spacing(10)
                    .align_y(Alignment::Start),
            )
            .padding([10, 14])
            .max_width(680)
            .style(style::card(accent));
            row![card, space::horizontal()].into()
        }

        (Direction::FromAgent, _) => {
            let mut footer = row![muted(clock(m.created_at)).size(style::fs(11))].spacing(6).align_y(Alignment::Center);
            if m.kind == MessageKind::Reply {
                footer = footer.push(tip(
                    container(muted("reply").size(style::fs(10))).padding([0, 6]).style(style::pill),
                    "The agent's final answer, mirrored from its session",
                ));
            }
            footer = footer.push(space::horizontal()).push(select_button(m.id, is_selecting)).push(copy_button(&m.body));
            let unread_mark: Element<'a, Msg> = if m.read_at.is_none() {
                dot(p.primary.base.color, 6.0)
            } else {
                Space::new().width(6).into()
            };
            let content: Element<'a, Msg> = if is_selecting && let Some(ed) = editor {
                message_editor(ed, m.id, theme)
            } else {
                selectable_body(md(items, &m.body, settings), m.id)
            };
            let bubble = container(column![content, footer].spacing(6))
                .padding([10, 14])
                .max_width(680)
                .style(style::bubble_agent);
            row![bubble, unread_mark, space::horizontal()].spacing(6).align_y(Alignment::Center).into()
        }
    }
}

fn question_view<'a>(
    app: &'a App,
    s: &'a Session,
    thread: &'a Thread,
    m: &'a Message,
    items: Option<&'a Vec<markdown::Item>>,
    settings: markdown::Settings,
    theme: &Theme,
) -> Element<'a, Msg> {
    let p = theme.extended_palette();
    let permission = m.kind == MessageKind::Permission;
    let accent = if permission { p.warning.base.color } else { p.primary.base.color };
    let label = if permission { "Permission request" } else { "Question" };
    let is_selecting = app.selecting_message == Some(m.id);
    let editor = thread.editors.get(&m.id);
    let content: Element<'a, Msg> = if is_selecting && let Some(ed) = editor {
        message_editor(ed, m.id, theme)
    } else {
        selectable_body(md(items, &m.body, settings), m.id)
    };

    let mut col = column![
        text(label).size(style::fs(12)).font(bold()).style(move |_: &Theme| text::Style { color: Some(accent) }),
        content,
    ]
    .spacing(8);

    match &m.question {
        Some(q) if q.status == QuestionStatus::Pending && s.status != SessionStatus::Ended => {
            if permission {
                let mut buttons = row![].spacing(8);
                for option in &q.options {
                    let style = match option.as_str() {
                        "Allow" | "Yes" => style::success_button,
                        "Deny" | "No" => style::danger_button,
                        _ => style::pill_button,
                    };
                    buttons = buttons.push(
                        button(text(option.as_str()).size(style::fs(13)))
                            .on_press(Msg::Answer { question: q.id.clone(), answer: option.clone() })
                            .style(style)
                            .padding([6, 14]),
                    );
                }
                col = col.push(buttons);
            } else {
                let mut buttons = column![].spacing(6).width(Fill);
                for option in &q.options {
                    let style = match option.as_str() {
                        "Allow" | "Yes" => style::success_button,
                        "Deny" | "No" => style::danger_button,
                        _ => style::pill_button,
                    };
                    buttons = buttons.push(
                        button(
                            text(option.as_str())
                                .size(style::fs(13))
                                .width(Fill)
                                .align_x(iced::alignment::Horizontal::Left)
                                .wrapping(iced::widget::text::Wrapping::Word),
                        )
                        .on_press(Msg::Answer { question: q.id.clone(), answer: option.clone() })
                        .style(style)
                        .padding([6, 14])
                        .width(Fill),
                    );
                }
                col = col.push(buttons);
                let draft = app.answers.get(&q.id).map(String::as_str).unwrap_or("");
                let qid = q.id.clone();
                let qid2 = q.id.clone();
                col = col.push(
                    row![
                        text_input("Type an answer…", draft)
                            .on_input(move |text| Msg::AnswerDraft { question: qid.clone(), text })
                            .on_submit(Msg::Answer { question: qid2.clone(), answer: draft.to_string() })
                            .size(style::fs(13))
                            .padding([6, 10])
                            .style(style::input),
                        button(text("Answer").size(style::fs(13)))
                            .on_press_maybe((!draft.trim().is_empty()).then(|| Msg::Answer {
                                question: q.id.clone(),
                                answer: draft.to_string(),
                            }))
                            .style(style::primary_button)
                            .padding([6, 12]),
                    ]
                    .spacing(8),
                );
            }
            if let Some(expires) = q.expires_at {
                col = col.push(muted(format!("Waiting for you until {}", clock(expires))).size(style::fs(11)));
            }
        }
        Some(q) if q.status == QuestionStatus::Answered => {
            col = col.push(
                text(format!("Answered: {}", q.answer.as_deref().unwrap_or("")))
                    .size(style::fs(13))
                    .style(|theme: &Theme| text::Style { color: Some(theme.extended_palette().success.base.color) }),
            );
        }
        Some(_) => {
            let note = if permission {
                "No answer in time — the agent asked in its terminal instead."
            } else {
                "No answer — the question expired."
            };
            col = col.push(muted(note).size(style::fs(12)));
        }
        None => {}
    }
    let footer = row![
        muted(clock(m.created_at)).size(style::fs(11)),
        space::horizontal(),
        select_button(m.id, is_selecting),
        copy_button(&m.body),
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    col = col.push(footer);

    let card = container(col).padding([12, 14]).max_width(680).style(style::card(accent));
    row![card, space::horizontal()].into()
}

fn composer<'a>(app: &'a App, s: &'a Session) -> Element<'a, Msg> {
    if s.status == SessionStatus::Ended {
        return container(muted("This conversation is read-only.").size(style::fs(13)))
            .padding([14, 20])
            .width(Fill)
            .style(style::composer_box)
            .into();
    }
    let placeholder = match s.status {
        SessionStatus::Active => "Message the agent — delivered after its current step…",
        SessionStatus::Idle if s.harness == Harness::ClaudeCode => "Message — delivered on the agent's next turn…",
        SessionStatus::Disconnected => "Message — kept until the agent reconnects…",
        _ => "Message the agent…",
    };
    let can_send = !app.composer.text().trim().is_empty();
    let editor = text_editor(&app.composer)
        .id("composer")
        .placeholder(placeholder)
        .on_action(Msg::Composer)
        .key_binding(|press| {
            use iced::keyboard::{Key, key::Named};
            if let Some(step) = crate::app::zoom_shortcut(&press.key, press.modifiers) {
                return Some(text_editor::Binding::Custom(Msg::Zoom(step)));
            }
            if let Some(msg) = crate::app::bar_shortcut(&press.key, press.modifiers) {
                return Some(text_editor::Binding::Custom(msg));
            }
            if matches!(press.key, Key::Named(Named::Enter)) && !press.modifiers.shift()
                && let text_editor::Status::Focused { .. } = press.status {
                    return Some(text_editor::Binding::Custom(Msg::Send));
                }
            text_editor::Binding::from_key_press(press)
        })
        .padding([10, 12])
        .size(style::fs(14))
        .min_height(44)
        .max_height(200)
        .height(Shrink)
        .style(style::editor);
    let send = button(text("Send").size(style::fs(14)).font(bold()))
        .on_press_maybe(can_send.then_some(Msg::Send))
        .style(style::primary_button)
        .padding([11, 20]);
    container(
        column![
            row![editor, send].spacing(10).align_y(Alignment::End),
            muted("Enter to send · Shift+Enter for a new line").size(style::fs(11)),
        ]
        .spacing(6),
    )
    .padding([12, 18])
    .width(Fill)
    .style(style::composer_box)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_fenced_code_block_with_language() {
        let md = "Here is some code:\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\nAll done.";
        let blocks = extract_code_blocks(md);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0], "fn main() {\n    println!(\"hello\");\n}\n");
    }

    #[test]
    fn extracts_fenced_code_block_without_language() {
        let md = "Plain block:\n```\njust plain text\n```";
        let blocks = extract_code_blocks(md);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0], "just plain text\n");
    }

    #[test]
    fn extracts_multiple_code_blocks_in_order() {
        let md = "First block:\n```bash\ncargo check\n```\nSecond block:\n```bash\ncargo test\n```";
        let blocks = extract_code_blocks(md);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], "cargo check\n");
        assert_eq!(blocks[1], "cargo test\n");
    }

    #[test]
    fn extracts_indented_code_blocks() {
        let md = "Indented:\n\n    let a = 1;\n    let b = 2;\n\nEnd.";
        let blocks = extract_code_blocks(md);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0], "let a = 1;\nlet b = 2;\n");
    }

    #[test]
    fn ignores_inline_code_and_plain_text() {
        let md = "Use `cargo test` to run tests. No blocks here!";
        let blocks = extract_code_blocks(md);
        assert!(blocks.is_empty());
    }

    #[test]
    fn handles_empty_message() {
        let blocks = extract_code_blocks("");
        assert!(blocks.is_empty());
    }

    #[test]
    fn preserves_indented_blocks_inside_quote_and_list() {
        let md = "> Quote line 1\n>\n>     let a = 1;\n>\n> Quote line 2\n\n- Item 1\n\n      let b = 2;\n\n- Item 2";
        let normalized = normalize_indented_code_blocks(md);
        assert!(!normalized.contains("\n```\nlet a = 1;\n```\n"));
        assert!(!normalized.contains("\n```\nlet b = 2;\n```\n"));

        let items = parse_markdown(md);
        let quotes = items.iter().filter(|i| matches!(i, markdown::Item::Quote(_))).count();
        let lists = items.iter().filter(|i| matches!(i, markdown::Item::List { .. })).count();
        let top_code_blocks = items.iter().filter(|i| matches!(i, markdown::Item::CodeBlock { .. })).count();

        assert_eq!(quotes, 1, "Quote should remain intact and not split");
        assert_eq!(lists, 1, "List should remain intact and not split");
        assert_eq!(top_code_blocks, 0, "Indented blocks inside containers should not become top-level blocks");
    }

    #[test]
    fn root_widget_tag_preserved_with_toast() {
        let (mut app, _) = App::new(None);
        let tag_without = {
            let without_toast = view(&app);
            without_toast.as_widget().tag()
        };

        app.toast("Copied", false);
        let tag_with = {
            let with_toast = view(&app);
            with_toast.as_widget().tag()
        };

        assert_eq!(
            tag_without, tag_with,
            "Root widget tag must not change when toast is shown, otherwise iced drops the widget tree and resets scroll positions"
        );
    }

    fn test_session(
        id: &str,
        role: Option<&str>,
        parent_id: Option<&str>,
        status: SessionStatus,
        archived: bool,
    ) -> Session {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "harness": Harness::ClaudeCode,
            "external_id": format!("ext-{id}"),
            "cwd": "/w/",
            "title": id,
            "status": status,
            "listen": false,
            "mirror": false,
            "archived": archived,
            "unread": 0,
            "queued": 0,
            "created_at": 1000,
            "updated_at": 2000,
            "last_seen_at": 2000,
            "awaiting_reply": false,
            "role": role,
            "parent_id": parent_id,
        }))
        .expect("test session")
    }

    #[test]
    fn sidebar_items_expands_and_collapses_workers() {
        let coord = test_session("c1", Some(jgt_core::COORDINATOR), None, SessionStatus::Active, false);
        let worker1 = test_session("w1", Some("coder"), Some("c1"), SessionStatus::Active, false);
        let worker2 = test_session("w2", Some("tester"), Some("c1"), SessionStatus::Idle, false);
        let visible = [&coord, &worker1, &worker2];

        // Expanded (default: collapsed set is empty)
        let collapsed = HashSet::new();
        let items = sidebar_items(&visible, |_| true, &collapsed);
        let ids: Vec<&str> = items.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["c1", "w1", "w2"], "Expanded coordinator shows all workers beneath it");

        // Collapsed
        let mut collapsed = HashSet::new();
        collapsed.insert("c1".to_string());
        let items = sidebar_items(&visible, |_| true, &collapsed);
        let ids: Vec<&str> = items.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["c1"], "Collapsed coordinator hides its workers");
    }

    #[test]
    fn sidebar_items_independent_coordinators() {
        let coord1 = test_session("c1", Some(jgt_core::COORDINATOR), None, SessionStatus::Active, false);
        let worker1 = test_session("w1", Some("coder"), Some("c1"), SessionStatus::Active, false);
        let coord2 = test_session("c2", Some(jgt_core::COORDINATOR), None, SessionStatus::Active, false);
        let worker2 = test_session("w2", Some("reviewer"), Some("c2"), SessionStatus::Active, false);
        let visible = [&coord1, &worker1, &coord2, &worker2];

        // Collapse only c1
        let mut collapsed = HashSet::new();
        collapsed.insert("c1".to_string());
        let items = sidebar_items(&visible, |_| true, &collapsed);
        let ids: Vec<&str> = items.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["c1", "c2", "w2"], "Only c1 workers are collapsed; c2 workers remain visible");
    }

    #[test]
    fn partition_team_workers_splits_active_and_idle() {
        let w_active = test_session("w1", Some("coder"), Some("c1"), SessionStatus::Active, false);
        let w_idle = test_session("w2", Some("tester"), Some("c1"), SessionStatus::Idle, false);
        let w_listen = test_session("w3", Some("tester"), Some("c1"), SessionStatus::Listening, false);
        let w_disc = test_session("w4", Some("tester"), Some("c1"), SessionStatus::Disconnected, false);
        let w_ended = test_session("w5", Some("tester"), Some("c1"), SessionStatus::Ended, false);
        let w_archived = test_session("w6", Some("tester"), Some("c1"), SessionStatus::Ended, true);

        let workers = [&w_active, &w_idle, &w_listen, &w_disc, &w_ended, &w_archived];

        // show_archived = false
        let (active, idle) = partition_team_workers(&workers, false);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "w1");
        assert_eq!(idle.len(), 4);
        let idle_ids: Vec<&str> = idle.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(idle_ids, vec!["w2", "w3", "w4", "w5"]);

        // show_archived = true
        let (active, idle) = partition_team_workers(&workers, true);
        assert_eq!(active.len(), 1);
        assert_eq!(idle.len(), 5);
        assert!(idle.iter().any(|s| s.id == "w6"));
    }

    #[test]
    fn test_listen_enabled() {
        let mut s = test_session("s1", Some("coder"), None, SessionStatus::Active, false);
        s.harness = Harness::ClaudeCode;
        assert!(listen_enabled(&s));

        s.status = SessionStatus::Listening;
        assert!(listen_enabled(&s));

        s.status = SessionStatus::Idle;
        assert!(listen_enabled(&s));

        s.status = SessionStatus::Ended;
        assert!(!listen_enabled(&s));

        s.status = SessionStatus::Active;
        s.harness = Harness::Custom;
        assert!(!listen_enabled(&s));

        s.status = SessionStatus::Ended;
        s.harness = Harness::Custom;
        assert!(!listen_enabled(&s));
    }

    #[test]
    fn count_coordinators_filters_correctly() {
        assert_eq!(count_coordinators(&[]), 0);

        let active_coord1 = test_session("c1", Some(jgt_core::COORDINATOR), None, SessionStatus::Active, false);
        let idle_coord2 = test_session("c2", Some(jgt_core::COORDINATOR), None, SessionStatus::Idle, false);
        let ended_coord = test_session("c3", Some(jgt_core::COORDINATOR), None, SessionStatus::Ended, false);
        let archived_coord = test_session("c4", Some(jgt_core::COORDINATOR), None, SessionStatus::Active, true);
        let worker_active = test_session("w1", Some("coder"), Some("c1"), SessionStatus::Active, false);
        let worker_archived = test_session("w2", Some("tester"), Some("c1"), SessionStatus::Ended, true);
        let no_role_session = test_session("s1", None, None, SessionStatus::Active, false);

        let excluded = [
            ended_coord.clone(),
            archived_coord.clone(),
            worker_active.clone(),
            worker_archived.clone(),
            no_role_session.clone(),
        ];
        assert_eq!(count_coordinators(&excluded), 0, "Archived, ended, and non-coordinator sessions are not counted");

        let one_coord = [
            active_coord1.clone(),
            ended_coord.clone(),
            archived_coord.clone(),
            worker_active.clone(),
        ];
        assert_eq!(count_coordinators(&one_coord), 1, "Only 1 non-archived, non-ended coordinator is counted");

        let two_coords = [
            active_coord1,
            idle_coord2,
            ended_coord,
            archived_coord,
            worker_active,
            worker_archived,
            no_role_session,
        ];
        assert_eq!(count_coordinators(&two_coords), 2, "Both active and idle coordinators are counted");
    }

    fn test_session_full(
        id: &str,
        role: Option<&str>,
        parent_id: Option<&str>,
        cwd: Option<&str>,
        status: SessionStatus,
        archived: bool,
        cost_usd: Option<f64>,
    ) -> Session {
        let mut s = test_session(id, role, parent_id, status, archived);
        s.cwd = cwd.map(str::to_string);
        s.cost_usd = cost_usd;
        s
    }

    #[test]
    fn team_summary_empty_and_unknown() {
        assert_eq!(team_summary(&[], "/projects/alpha"), (0, 0.0));
        assert_eq!(team_summary(&[], ""), (0, 0.0));

        let c1 = test_session_full("c1", Some(jgt_core::COORDINATOR), None, Some("/projects/alpha"), SessionStatus::Active, false, None);
        assert_eq!(team_summary(std::slice::from_ref(&c1), "/projects/beta"), (0, 0.0));
        assert_eq!(team_summary(&[c1], "/projects/alpha"), (0, 0.0), "Coordinator without workers has 0 working and 0.0 cost");
    }

    #[test]
    fn team_summary_counts_working_and_sums_cost_including_ended_and_archived() {
        let c1 = test_session_full("c1", Some(jgt_core::COORDINATOR), None, Some("/team1"), SessionStatus::Active, false, None);
        let w_act = test_session_full("w1", Some("coder"), Some("c1"), Some("/team1"), SessionStatus::Active, false, Some(0.25));
        let w_list = test_session_full("w2", Some("tester"), Some("c1"), Some("/team1"), SessionStatus::Listening, false, Some(0.15));
        let w_idle = test_session_full("w3", Some("reviewer"), Some("c1"), Some("/team1"), SessionStatus::Idle, false, Some(0.10));
        let w_disc = test_session_full("w4", Some("coder"), Some("c1"), Some("/team1"), SessionStatus::Disconnected, false, Some(0.05));
        let w_ended = test_session_full("w5", Some("coder"), Some("c1"), Some("/team1"), SessionStatus::Ended, false, Some(0.42));
        let w_arch_ended = test_session_full("w6", Some("coder"), Some("c1"), Some("/team1"), SessionStatus::Ended, true, Some(0.08));
        let w_arch_act = test_session_full("w7", Some("coder"), Some("c1"), Some("/team1"), SessionStatus::Active, true, Some(0.20));

        // Other team sessions that should not be counted
        let c2 = test_session_full("c2", Some(jgt_core::COORDINATOR), None, Some("/team2"), SessionStatus::Active, false, None);
        let w_other = test_session_full("w_other", Some("coder"), Some("c2"), Some("/team2"), SessionStatus::Active, false, Some(9.99));
        let s_solo = test_session_full("s_solo", None, None, Some("/team1"), SessionStatus::Active, false, Some(5.00));

        let sessions = vec![c1, w_act, w_list, w_idle, w_disc, w_ended, w_arch_ended, w_arch_act, c2, w_other, s_solo];

        let (working, cost) = team_summary(&sessions, "/team1");
        // Working workers: w1 (Active), w2 (Listening), w3 (Idle), w7 (Archived Active) = 4
        assert_eq!(working, 4, "Working workers must include connected statuses (Active, Listening, Idle) including archived active ones");
        // Cost: 0.25 + 0.15 + 0.10 + 0.05 + 0.42 + 0.08 + 0.20 = 1.25
        assert!((cost - 1.25).abs() < 1e-6, "Cost must sum all workers of the coordinator, including ended and archived");

        // Normalization: trailing slash should produce identical results
        let (working_norm, cost_norm) = team_summary(&sessions, "/team1/");
        assert_eq!(working_norm, working);
        assert!((cost_norm - cost).abs() < 1e-6);

        // Check team2
        let (t2_working, t2_cost) = team_summary(&sessions, "/team2");
        assert_eq!(t2_working, 1);
        assert!((t2_cost - 9.99).abs() < 1e-6);
    }

    #[test]
    fn workspace_choice_display() {
        let team = WorkspaceChoice::Team {
            key: "/projects/alpha".to_string(),
            name: "alpha".to_string(),
        };
        assert_eq!(team.to_string(), "alpha");

        let new_team = WorkspaceChoice::NewTeam;
        assert_eq!(new_team.to_string(), "New team…");
    }

    #[test]
    fn app_header_and_view_render() {
        let (mut app, _) = App::new(None);
        let _ = app_header(&app);
        let _ = view(&app);

        // With settings open
        app.show_settings = true;
        let _ = view(&app);

        // With archived chats shown
        app.show_archived = true;
        let _ = app_header(&app);

        // With a selected workspace and session
        let c1 = test_session_full("c1", Some(jgt_core::COORDINATOR), None, Some("/team1"), SessionStatus::Active, false, None);
        let w1 = test_session_full("w1", Some("coder"), Some("c1"), Some("/team1"), SessionStatus::Active, false, Some(0.42));
        app.sessions = vec![c1, w1];
        app.workspace = Some("/team1".to_string());
        let _ = app_header(&app);
        let _ = view(&app);
    }

    #[test]
    fn test_sidebar_workspace_model() {
        let c1 = test_session_full("c1", Some(jgt_core::COORDINATOR), None, Some("/team1"), SessionStatus::Active, false, None);
        let w_act = test_session_full("w_act", Some("coder"), Some("c1"), Some("/team1"), SessionStatus::Active, false, Some(0.5));
        let w_idle = test_session_full("w_idle", Some("tester"), Some("c1"), Some("/team1"), SessionStatus::Idle, false, Some(0.2));
        let sessions = vec![c1, w_act, w_idle];

        let mut boards = HashMap::new();

        // 1. Board not loaded yet: board_open_tasks is None
        let model_unloaded = sidebar_workspace_model(&sessions, &boards, "/team1", false);
        assert_eq!(model_unloaded.board_open_tasks, None);
        assert_eq!(model_unloaded.coordinator.map(|s| s.id.as_str()), Some("c1"));
        assert_eq!(model_unloaded.workers.len(), 2);
        assert_eq!(model_unloaded.workers[0].id, "w_act");
        assert_eq!(model_unloaded.workers[1].id, "w_idle");
        assert!(!model_unloaded.workers_collapsed);

        // 2. Board loaded with 3 tasks: 2 open, 1 done -> count must be 2
        let t1 = jgt_core::Task::new("/team1", 1, "task 1", jgt_core::TaskStatus::Backlog);
        let t2 = jgt_core::Task::new("/team1", 2, "task 2", jgt_core::TaskStatus::Working);
        let t3 = jgt_core::Task::new("/team1", 3, "task 3", jgt_core::TaskStatus::Done);
        let board = crate::board::Board::new(vec![t1, t2, t3]);
        boards.insert("/team1".to_string(), board);

        let model_loaded = sidebar_workspace_model(&sessions, &boards, "/team1", true);
        assert_eq!(model_loaded.board_open_tasks, Some(2));
        assert_eq!(model_loaded.coordinator.map(|s| s.id.as_str()), Some("c1"));
        assert!(model_loaded.workers_collapsed);
    }

    #[test]
    fn test_board_render_without_selected_coordinator() {
        let (mut app, _) = App::new(None);
        app.workspace = Some("/team1".to_string());
        app.selected = None;
        app.module = Module::Board;

        let t1 = jgt_core::Task::new("/team1", 1, "task 1", jgt_core::TaskStatus::Backlog);
        let board = crate::board::Board::new(vec![t1]);
        app.boards.insert("/team1".to_string(), board);

        let theme = app.theme();
        // Render board directly with workspace without session
        let _ = crate::board::board(&app, "/team1", &theme);

        // Render main view and full view
        let _ = main(&app, &theme);
        let _ = view(&app);
        let _ = sidebar(&app, &theme);
    }

    #[test]
    fn test_collapsed_side_bars_render() {
        let (mut app, _) = App::new(None);
        let c1 = test_session_full("c1", Some(jgt_core::COORDINATOR), None, Some("/team1"), SessionStatus::Active, false, None);
        app.sessions = vec![c1];
        app.workspace = Some("/team1".to_string());
        app.selected = Some("c1".to_string());
        app.search = "c".to_string();

        for module in [Module::Coordinator, Module::Board, Module::Ide] {
            app.module = module;
            for (left, right) in [(true, false), (false, true), (true, true), (false, false)] {
                app.left_collapsed = left;
                app.right_collapsed = right;
                let _ = view(&app);
            }
        }
        // Collapsing only changes what is rendered.
        assert_eq!(app.selected.as_deref(), Some("c1"));
        assert_eq!(app.search, "c");

        for side in [Side::Left, Side::Right] {
            let _ = collapsed_bar(side);
            let _ = bar_toggle(side, false);
            let _ = bar_toggle(side, true);
        }
    }

    #[test]
    fn test_sidebar_modules_render() {
        let (mut app, _) = App::new(None);
        let c1 = test_session_full("c1", Some(jgt_core::COORDINATOR), None, Some("/team1"), SessionStatus::Active, false, None);
        let w1 = test_session_full("w1", Some("coder"), Some("c1"), Some("/team1"), SessionStatus::Active, false, Some(0.42));
        app.sessions = vec![c1, w1];
        app.workspace = Some("/team1".to_string());
        app.module = Module::Coordinator;

        let theme = app.theme();

        // 1. Expanded workers
        app.workers_collapsed = false;
        let _ = sidebar(&app, &theme);

        // 2. Collapsed workers
        app.workers_collapsed = true;
        let _ = sidebar(&app, &theme);

        // 3. Board module selected
        app.module = Module::Board;
        let _ = sidebar(&app, &theme);

        // 3b. Ide module selected
        app.module = Module::Ide;
        let _ = sidebar(&app, &theme);
        let _ = main(&app, &theme);

        // 4. Worker module selected
        app.module = Module::Worker("w1".to_string());
        let _ = sidebar(&app, &theme);

        // 5. Search active
        app.search = "coder".to_string();
        let _ = sidebar(&app, &theme);

        // 6. Archived view active
        app.search.clear();
        app.show_archived = true;
        let _ = sidebar(&app, &theme);
    }

    #[test]
    fn test_settings_agents_unlocked_renders() {
        let (mut app, _) = App::new(None);
        app.show_settings = true;
        app.installed_harnesses = vec![Harness::ClaudeCode, Harness::Antigravity, Harness::Opencode];
        app.agents = AGENT_ROLES
            .iter()
            .map(|&role| AgentRow {
                role: role.to_string(),
                harness: if role == jgt_core::COORDINATOR {
                    Harness::ClaudeCode
                } else if role == "reviewer" {
                    Harness::Opencode
                } else {
                    Harness::Antigravity
                },
                model: if role == jgt_core::COORDINATOR {
                    "opus".into()
                } else {
                    "gemini-3.8-flash-high".into()
                },
                effort: "high".into(),
            })
            .collect();

        app.harness_models.insert(
            Harness::ClaudeCode,
            Ok(vec!["opus".into(), "sonnet".into(), "fable".into(), "haiku".into()]),
        );
        app.harness_models.insert(
            Harness::Antigravity,
            Ok(vec!["gemini-3.8-flash-high".into()]),
        );

        assert!(app.agents_locked.is_none());
        assert_eq!(HarnessChoice(Harness::ClaudeCode).to_string(), "Claude Code");
        assert_eq!(HarnessChoice(Harness::Antigravity).to_string(), "Antigravity");

        let _ = agents_section(&app);
        let _ = settings(&app);
        let _ = view(&app);
    }

    #[test]
    fn test_settings_agents_locked_renders() {
        use jgt_core::TeamAgents;

        let (mut app, _) = App::new(None);
        app.show_settings = true;

        let mut roles = std::collections::BTreeMap::new();
        for &role in AGENT_ROLES {
            roles.insert(
                role.to_string(),
                jgt_core::AgentSetting {
                    harness: if role == jgt_core::COORDINATOR {
                        Harness::ClaudeCode
                    } else {
                        Harness::Antigravity
                    },
                    model: if role == jgt_core::COORDINATOR {
                        "opus".into()
                    } else {
                        "gemini-3.8-flash-high".into()
                    },
                    effort: "medium".into(),
                },
            );
        }
        app.agents_locked = Some(TeamAgents { roles });

        assert!(app.agents_locked.is_some());
        let _ = agents_section(&app);
        let _ = settings(&app);
        let _ = view(&app);
    }

    #[test]
    fn test_settings_agents_loading_and_error_states() {
        let (mut app, _) = App::new(None);
        app.show_settings = true;
        app.installed_harnesses = vec![Harness::ClaudeCode, Harness::Opencode];

        app.agents = vec![
            AgentRow {
                role: "coordinator".into(),
                harness: Harness::ClaudeCode,
                model: "opus".into(),
                effort: "medium".into(),
            },
            AgentRow {
                role: "reviewer".into(),
                harness: Harness::Opencode,
                model: "custom-model".into(),
                effort: "low".into(),
            },
            AgentRow {
                role: "coder".into(),
                harness: Harness::Custom,
                model: "unknown".into(),
                effort: "high".into(),
            },
        ];

        // Opencode returned an error
        app.harness_models.insert(
            Harness::Opencode,
            Err("opencode not found".into()),
        );
        // Custom is None (still loading)
        // ClaudeCode returned empty suggestions
        app.harness_models.insert(
            Harness::ClaudeCode,
            Ok(vec![]),
        );

        let _ = agents_section(&app);
        let _ = settings(&app);
        let _ = view(&app);
    }

    #[test]
    fn test_settings_agents_row_width_fits_card() {
        for fs14 in [14.0, style::fs(14)] {
            let role_width = fs14 * 7.0;
            let harness_width = fs14 * 9.5;
            let model_width = fs14 * 8.0;
            let effort_width = fs14 * 5.5;
            let suggestions_width = fs14 * 8.5;
            let spacing = 8.0 * 4.0;

            let total_row_width = role_width + harness_width + model_width + effort_width + suggestions_width + spacing;
            let card_max_width = fs14 * 46.0;
            let card_padding = 22.0 * 2.0;
            let scrollable_padding_right = 8.0;
            let scrollbar_width = 12.0;
            let usable_width = card_max_width - card_padding - scrollable_padding_right - scrollbar_width;

            assert!(
                total_row_width <= usable_width,
                "Row width ({total_row_width}px) exceeds usable width ({usable_width}px) at fs(14)={fs14}"
            );
        }
    }

    #[test]
    fn test_question_view_renders_short_and_long_options_and_permissions() {
        let (app, _) = App::new(None);
        let s = test_session("s1", Some("coordinator"), None, SessionStatus::Active, false);
        let thread = Thread::default();
        let theme = app.theme();
        let mut md_style = markdown::Style::from_palette(theme.palette());
        md_style.font = style::ui_font();
        let settings = markdown::Settings::with_text_size(style::fs(14), md_style);

        let make_msg = |id: i64, kind: MessageKind, q: Option<jgt_core::Question>| Message {
            id,
            session_id: "s1".to_string(),
            direction: Direction::FromAgent,
            kind,
            body: "Test message body".to_string(),
            created_at: 1000,
            delivered_at: None,
            delivery: Delivery::Delivered,
            read_at: None,
            meta: serde_json::Value::Null,
            question: q,
        };

        let make_q = |id: &str, kind: MessageKind, options: Vec<&str>| jgt_core::Question {
            id: id.to_string(),
            session_id: "s1".to_string(),
            message_id: 1,
            options: options.into_iter().map(String::from).collect(),
            status: QuestionStatus::Pending,
            answer: None,
            answered_at: None,
            created_at: 1000,
            expires_at: None,
            prompt: "Prompt".to_string(),
            kind,
        };

        // 1. Permission message (options in a row)
        let perm_q = make_q("q1", MessageKind::Permission, vec!["Allow", "Deny"]);
        let perm_msg = make_msg(1, MessageKind::Permission, Some(perm_q));
        let _ = question_view(&app, &s, &thread, &perm_msg, None, settings, &theme);

        // 2. Question with long pre-made options (column with wrapping text)
        let long_q = make_q(
            "q2",
            MessageKind::Question,
            vec![
                "Strategy A: Make the options wrap/stack so long labels are fully readable inside the card with Fill width",
                "Strategy B: Implement a custom wrapping layout widget that measures text dimensions dynamically",
            ],
        );
        let long_q_msg = make_msg(2, MessageKind::Question, Some(long_q));
        let _ = question_view(&app, &s, &thread, &long_q_msg, None, settings, &theme);

        // 3. Question with short options
        let short_q = make_q("q3", MessageKind::Question, vec!["Yes", "No"]);
        let short_q_msg = make_msg(3, MessageKind::Question, Some(short_q));
        let _ = question_view(&app, &s, &thread, &short_q_msg, None, settings, &theme);
    }

    #[test]
    fn test_message_view_renders_unselected_and_selected() {
        let (mut app, _) = App::new(None);
        let s = test_session("s1", Some("coordinator"), None, SessionStatus::Active, false);
        let mut thread = Thread::default();

        let agent_msg = Message {
            id: 10,
            session_id: "s1".to_string(),
            direction: Direction::FromAgent,
            kind: MessageKind::Text,
            body: "```rust\nfn foo() {}\n```".to_string(),
            created_at: 1000,
            delivered_at: None,
            delivery: Delivery::Delivered,
            read_at: None,
            meta: serde_json::Value::Null,
            question: None,
        };
        let user_msg = Message {
            id: 11,
            session_id: "s1".to_string(),
            direction: Direction::ToAgent,
            kind: MessageKind::Text,
            body: "User message body".to_string(),
            created_at: 1001,
            delivered_at: None,
            delivery: Delivery::Delivered,
            read_at: None,
            meta: serde_json::Value::Null,
            question: None,
        };
        let notify_msg = Message {
            id: 12,
            session_id: "s1".to_string(),
            direction: Direction::FromAgent,
            kind: MessageKind::Notify,
            body: "Notification body".to_string(),
            created_at: 1002,
            delivered_at: None,
            delivery: Delivery::Delivered,
            read_at: None,
            meta: serde_json::Value::Null,
            question: None,
        };

        thread.upsert(agent_msg.clone());
        thread.upsert(user_msg.clone());
        thread.upsert(notify_msg.clone());

        for theme in [Theme::Dark, Theme::Light] {
            let mut md_style = markdown::Style::from_palette(theme.palette());
            md_style.font = style::ui_font();
            let settings = markdown::Settings::with_text_size(style::fs(14), md_style);

            // Unselected rendering
            app.selecting_message = None;
            let _ = message_view(&app, &s, &thread, &agent_msg, thread.markdown.get(&10), settings, &theme);
            let _ = message_view(&app, &s, &thread, &user_msg, thread.markdown.get(&11), settings, &theme);
            let _ = message_view(&app, &s, &thread, &notify_msg, thread.markdown.get(&12), settings, &theme);

            // Selected rendering (enters text_editor view with code highlighting)
            app.selecting_message = Some(10);
            let _ = message_view(&app, &s, &thread, &agent_msg, thread.markdown.get(&10), settings, &theme);
            app.selecting_message = Some(11);
            let _ = message_view(&app, &s, &thread, &user_msg, thread.markdown.get(&11), settings, &theme);
            app.selecting_message = Some(12);
            let _ = message_view(&app, &s, &thread, &notify_msg, thread.markdown.get(&12), settings, &theme);
        }
    }
}

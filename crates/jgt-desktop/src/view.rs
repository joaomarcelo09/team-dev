//! Everything on screen: the session sidebar, the chat, the composer.

use crate::{
    app::{App, Conn, MESSAGES_ID, Msg, Tab, ThemePref, Thread},
    style,
};
use chrono::{Datelike, Local, TimeZone};
use iced::{
    Alignment, Color, Element, Fill, Font, Padding, Shrink, Theme,
    font::Weight,
    widget::{
        Space, button, column, container, markdown, row, rule, scrollable, space, stack, text, text_editor,
        text_input, toggler, tooltip,
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

pub fn view(app: &App) -> Element<'_, Msg> {
    let theme = app.theme();
    let body = row![sidebar(app, &theme), rule::vertical(1).style(rule::weak), main(app, &theme)];
    let base: Element<'_, Msg> = if app.show_settings {
        stack![container(body).width(Fill).height(Fill), settings(app)].into()
    } else {
        container(body).width(Fill).height(Fill).into()
    };

    match &app.toast {
        Some(toast) => {
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
            stack![base, container(card).padding(18).width(Fill).height(Fill).align_right(Fill).align_bottom(Fill)]
                .into()
        }
        None => base,
    }
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
    let line = |label: &'static str, control: Element<'static, Msg>| {
        row![text(label).size(style::fs(14)).width(style::fs(14) * 7.0), control].spacing(12).align_y(Alignment::Center)
    };
    let card = container(
        column![
            row![
                text("Settings").size(style::fs(18)).font(bold()),
                space::horizontal(),
                button(text("Close").size(style::fs(13))).on_press(Msg::ToggleSettings).style(style::ghost).padding([4, 8]),
            ]
            .align_y(Alignment::Center),
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
            config_banner(app),
        ]
        .spacing(16),
    )
    .padding(22)
    .max_width(style::fs(14) * 34.0)
    .style(style::card(Color::TRANSPARENT));
    let backdrop = iced::widget::mouse_area(
        container(Space::new()).width(Fill).height(Fill).style(|_: &Theme| container::Style {
            background: Some(Color { a: 0.45, ..Color::BLACK }.into()),
            ..Default::default()
        }),
    )
    .on_press(Msg::ToggleSettings);
    stack![backdrop, container(iced::widget::opaque(card)).center(Fill)].into()
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

// ── sidebar ─────────────────────────────────────────────────────────────────

fn sidebar<'a>(app: &'a App, theme: &Theme) -> Element<'a, Msg> {
    let p = theme.extended_palette();
    let (conn_color, conn_label) = match &app.conn {
        Conn::Connected => (p.success.base.color, "Connected to the JG Team Dev hub".to_string()),
        Conn::Connecting => (p.warning.base.color, "Connecting to the hub…".to_string()),
        Conn::Offline(e) => (p.danger.base.color, format!("Hub not running: {e}")),
    };
    let header = row![
        text(jgt_core::APP_NAME).size(style::fs(17)).font(bold()),
        tooltip(
            dot(conn_color, 8.0),
            container(text(conn_label).size(style::fs(12))).padding([6, 10]).style(container::rounded_box),
            tooltip::Position::Bottom
        ),
        space::horizontal(),
        tip(
            button(text("Settings").size(style::fs(12)))
                .on_press(Msg::ToggleSettings)
                .style(if app.show_settings { style::pill_button } else { style::ghost })
                .padding([4, 8]),
            "Theme, zoom and text size"
        ),
        tip(
            button(text("Archived").size(style::fs(12)))
                .on_press(Msg::ToggleArchived)
                .style(if app.show_archived { style::pill_button } else { style::ghost })
                .padding([4, 8]),
            if app.show_archived { "Hide archived chats" } else { "Show archived chats" }
        ),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let search = text_input("Search chats", &app.search)
        .on_input(Msg::Search)
        .padding([7, 10])
        .size(style::fs(13))
        .style(style::input);

    let query = app.search.trim().to_lowercase();
    let matches = |s: &&Session| {
        query.is_empty()
            || s.display_name().to_lowercase().contains(&query)
            || s.cwd.as_deref().unwrap_or("").to_lowercase().contains(&query)
            || s.harness.label().to_lowercase().contains(&query)
    };
    let visible: Vec<&Session> = app.sessions.iter().filter(matches).collect();

    let groups: [(&str, Box<dyn Fn(&Session) -> bool>); 4] = [
        ("ONLINE", Box::new(|s: &Session| !s.archived && s.status.is_connected())),
        ("OFFLINE", Box::new(|s: &Session| !s.archived && s.status == SessionStatus::Disconnected)),
        ("ENDED", Box::new(|s: &Session| !s.archived && s.status == SessionStatus::Ended)),
        ("ARCHIVED", Box::new(|s: &Session| s.archived)),
    ];
    let mut list = column![].spacing(2);
    let mut any = false;
    for (label, include) in groups.iter() {
        if *label == "ARCHIVED" && !app.show_archived {
            continue;
        }
        // Workers sit right under their Coordinator, whatever their own state; only a worker whose
        // Coordinator is not listed is grouped by its own state.
        let parent_listed = |w: &Session| visible.iter().any(|p| Some(p.id.as_str()) == w.parent_id.as_deref());
        let mut items: Vec<&&Session> = Vec::new();
        for s in visible.iter().filter(|s| include(s) && !(s.is_worker() && parent_listed(s))) {
            items.push(s);
            if !s.is_worker() {
                items.extend(visible.iter().filter(|w| w.is_worker() && w.parent_id.as_deref() == Some(s.id.as_str())));
            }
        }
        if items.is_empty() {
            continue;
        }
        any = true;
        list = list.push(
            container(muted(format!("{label} · {}", items.len())).size(style::fs(11)).font(bold())).padding([10, 8]),
        );
        for s in items {
            let item = session_item(s, app.selected.as_deref() == Some(s.id.as_str()), theme);
            list = list.push(if s.is_worker() { container(item).padding(Padding::ZERO.left(16)).into() } else { item });
        }
    }
    if !any {
        let hint = if app.sessions.is_empty() {
            "No team sessions yet.\n\nClick New team and pick a project folder: its Coordinator starts and greets you here."
        } else {
            "Nothing matches."
        };
        list = list.push(container(muted(hint).size(style::fs(13))).padding(12));
    }

    let new_team: Element<'a, Msg> = match &app.new_team {
        Some(folder) => {
            let input = text_input("Project folder", folder)
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
                    // `~/font/app` reads better than the full path in a narrow sidebar.
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
            container(form)
            .padding([0, 12])
            .into()
        }
        None => container(
            button(text("New team").size(style::fs(13)).width(Fill).align_x(Alignment::Center))
                .on_press(Msg::NewTeamOpen)
                .style(style::primary_button)
                .padding([6, 10])
                .width(Fill),
        )
        .padding([0, 12])
        .into(),
    };

    container(
        column![
            container(header).padding([14, 14]),
            new_team,
            container(search).padding([0, 12]),
            scrollable(container(list).padding([6, 8])).height(Fill),
        ]
        .spacing(8),
    )
    // Grows with the text size (310px at the default 16px).
    .width(style::fs(14) * 310.0 / f32::from(jgt_core::config::Desktop::DEFAULT_TEXT_SIZE))
    .height(Fill)
    .style(style::sidebar)
    .into()
}

/// The folder typed or picked in the New team form.
fn folder_value(app: &App) -> &str {
    app.new_team.as_deref().unwrap_or("").trim_end_matches('/')
}

fn session_item<'a>(s: &'a Session, selected: bool, theme: &Theme) -> Element<'a, Msg> {
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
    let content = column![
        row![
            text(s.display_name()).size(style::fs(14)).font(name_font).wrapping(text::Wrapping::None),
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
    let Some(session) = app.selected_session() else { return welcome(app) };
    if has_board(session) && app.tab(&session.id) == Tab::Board {
        return column![chat_header(app, session, theme), rule::horizontal(1).style(rule::weak), crate::board::board(app, session, theme)]
            .width(Fill)
            .height(Fill)
            .into();
    }
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

fn welcome(app: &App) -> Element<'_, Msg> {
    let status = match &app.conn {
        Conn::Connected => format!(
            "{} session{} · {} online",
            app.sessions.len(),
            if app.sessions.len() == 1 { "" } else { "s" },
            app.sessions.iter().filter(|s| s.status.is_connected()).count()
        ),
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

    let listen = tip(
        toggler(s.listen).label("Listen").text_size(style::fs(13)).size(16).on_toggle_maybe(
            (s.status != SessionStatus::Ended && s.harness != Harness::Custom).then_some(Msg::SetListen),
        ),
        "When its turn ends, the agent keeps waiting for your messages instead of going idle",
    );
    let mirror = tip(
        toggler(s.mirror).label("All replies").text_size(style::fs(13)).size(16).on_toggle(Msg::SetMirror),
        "Show every final reply here, not only replies to messages you sent from JG Team Dev",
    );
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

    let mut controls = row![].spacing(14).align_y(Alignment::Center);
    if has_board(s) {
        let tab = app.tab(&s.id);
        let tab_button = |label: &'static str, which: Tab| {
            button(text(label).size(style::fs(13)).font(if tab == which { bold() } else { style::ui_font() }))
                .on_press(Msg::SetTab(which))
                .style(style::tab(tab == which))
                .padding([6, 12])
        };
        controls = controls.push(row![tab_button("Chat", Tab::Chat), tab_button("Board", Tab::Board)].spacing(2));
    }
    let controls = controls.push(listen).push(mirror).push(archive).push(delete);
    // The title side takes what the controls leave and clips (long paths, large text).
    let top = row![column![title, subtitle].spacing(4).width(Fill).clip(true), controls]
        .spacing(16)
        .align_y(Alignment::Center);

    let mut col = column![top].spacing(10);
    if let Some(banner) = status_banner(app, s, theme) {
        col = col.push(banner);
    }
    if let Some(team) = team_panel(app, s, theme) {
        col = col.push(team);
    }
    container(col).padding([12, 18]).width(Fill).style(style::header).into()
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

/// Coordinators with a folder get the Chat | Board tabs.
fn has_board(s: &Session) -> bool {
    crate::board::workspace(s).is_some()
}

/// `1m05s`, `2h03m`.
fn duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs >= 3600 { format!("{}h{:02}m", secs / 3600, secs % 3600 / 60) } else { format!("{}m{:02}s", secs / 60, secs % 60) }
}

/// A Coordinator's workers: what each is doing, for how long, and what it cost.
fn team_panel<'a>(app: &'a App, s: &'a Session, theme: &Theme) -> Option<Element<'a, Msg>> {
    if !s.is_coordinator() {
        return None;
    }
    let workers: Vec<&Session> = app.sessions.iter().filter(|w| w.parent_id.as_deref() == Some(s.id.as_str())).collect();
    if workers.is_empty() {
        return None;
    }
    let now = jgt_core::now_ms();
    let shown: Vec<&&Session> = workers.iter().filter(|w| !w.archived || app.show_archived).collect();
    let archived = workers.iter().filter(|w| w.archived).count();
    let working = workers.iter().filter(|w| w.status.is_connected()).count();
    let cost: f64 = workers.iter().filter_map(|w| w.cost_usd).sum();

    let mut summary = row![
        muted("TEAM").size(style::fs(11)).font(bold()),
        muted(format!("{working} working · {} done · ${cost:.2}", workers.len() - working)).size(style::fs(11)),
        space::horizontal(),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    if archived > 0 {
        let label = if app.show_archived { "Hide archived".to_string() } else { format!("{archived} archived") };
        summary = summary.push(
            tip(
                button(text(label).size(style::fs(11))).on_press(Msg::ToggleArchived).style(style::ghost).padding([1, 6]),
                "Worker chats of finished runs are archived: kept, but hidden",
            ),
        );
    }

    let mut list = column![summary].spacing(2);
    for w in shown {
        let (state, secs) = match w.status {
            SessionStatus::Active => ("working", (now - w.created_at) / 1000),
            SessionStatus::Idle | SessionStatus::Listening => ("idle", (now - w.created_at) / 1000),
            _ => (
                if w.status == SessionStatus::Ended { "ended" } else { "done" },
                w.run_secs.unwrap_or((w.updated_at - w.created_at) / 1000),
            ),
        };
        let detail = if w.status.is_connected() { w.status_text.clone().unwrap_or_default() } else { String::new() };
        // A finished run whose harness reports no cost shows "—" (PRD H8).
        let cost = match w.cost_usd {
            Some(c) => format!("${c:.2}"),
            None if w.status.is_connected() => String::new(),
            None => "—".to_string(),
        };
        let runs_on = [Some(w.harness.label().to_string()), w.model.clone()].into_iter().flatten().collect::<Vec<_>>();
        let line = row![
            dot(style::status_color(theme, w.status), 7.0),
            text(w.display_name()).size(style::fs(12)).wrapping(text::Wrapping::None).width(Fill),
            muted(detail).size(style::fs(11)).wrapping(text::Wrapping::None),
            container(text(runs_on.join(" · ")).size(style::fs(10)).wrapping(text::Wrapping::None))
                .padding([1, 6])
                .style(style::pill),
            muted(state).size(style::fs(11)).width(56),
            muted(duration(secs)).size(style::fs(11)).width(56),
            muted(cost).size(style::fs(11)).width(48),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        list = list.push(
            button(container(line).clip(true))
                .on_press(Msg::Select(w.id.clone()))
                .style(style::session_item(false))
                .width(Fill)
                .padding([3, 8]),
        );
    }
    Some(container(list).padding([6, 8]).width(Fill).style(style::pill).into())
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

    let parser = Parser::new(body).into_offset_iter();
    for (event, range) in parser {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Indented)) => {
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

fn message_editor<'a>(editor: &'a text_editor::Content, id: i64) -> Element<'a, Msg> {
    text_editor(editor)
        .on_action(move |action| Msg::MessageEditor(id, action))
        .padding([8, 10])
        .size(style::fs(14))
        .font(style::ui_font())
        .wrapping(iced::widget::text::Wrapping::Word)
        .style(style::read_only_editor)
        .into()
}

fn message_user_editor<'a>(editor: &'a text_editor::Content, id: i64) -> Element<'a, Msg> {
    text_editor(editor)
        .on_action(move |action| Msg::MessageEditor(id, action))
        .padding([8, 10])
        .size(style::fs(14))
        .font(style::ui_font())
        .wrapping(iced::widget::text::Wrapping::Word)
        .style(style::read_only_user_editor)
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
                select_button(m.id, is_selecting),
                copy_button(&m.body),
            ]
            .spacing(6)
            .align_y(Alignment::Center);
            let content: Element<'a, Msg> = if is_selecting && let Some(ed) = editor {
                message_user_editor(ed, m.id)
            } else {
                text(m.body.as_str()).size(style::fs(14)).into()
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
                message_editor(ed, m.id)
            } else {
                md(items, &m.body, settings)
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
                message_editor(ed, m.id)
            } else {
                md(items, &m.body, settings)
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
        message_editor(ed, m.id)
    } else {
        md(items, &m.body, settings)
    };

    let mut col = column![
        text(label).size(style::fs(12)).font(bold()).style(move |_: &Theme| text::Style { color: Some(accent) }),
        content,
    ]
    .spacing(8);

    match &m.question {
        Some(q) if q.status == QuestionStatus::Pending && s.status != SessionStatus::Ended => {
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
            if !permission {
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
}

//! The Dark Factory window: a header of tabs over the latest lab snapshot.
//!
//! The Chat tab is the user's conversation with the Lab Coordinator: it shows the merged
//! mailboxes, sends a draft through the router and acks the user's mailbox when opened. The
//! Board tab is the read-only Board, the Lab tab shows the lab state and its usage meters, and
//! the Settings tab edits the role configuration. The polling shell reloads a [`LabSnapshot`]
//! from the data directory every two seconds, and read, send or save errors show in a
//! dismissible banner.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use iced::{
    Alignment, Element, Fill, Subscription, Task, Theme,
    widget::{
        button, column, container, pick_list, progress_bar, row, scrollable, space, text,
        text_input,
    },
};
use jg_ui::{ThemePref, ThemeStyle};
use jgf_core::{FactoryRole, paths, roles::RoleConfig};
use jgf_desktop::{
    board_view::{CardView, ColumnView, build_board, filter_choices, filter_for, project_choices},
    chat_view::{ChatLine, subject_from, validate_draft},
    lab_view::{LabView, UsageRow, build_lab_view},
    settings_view::{
        FieldEdit, SettingsForm, SettingsRow, apply_edit, effort_options, harness_options,
    },
    snapshot::{CoordinatorState, LAB_ID, LabSnapshot, ROLES_FILE},
};
use jgf_hub::{
    MessageKind, OutgoingMessage, Role, Router,
    board::Column,
    usage::{LabState, Level},
};

use crate::{
    appearance::{self, Appearance},
    style,
};

/// Seconds between snapshot reloads.
const TICK_SECS: u64 = 2;

/// Routing store directory under the data directory, passed to `Router::open`.
const ROUTING_DIR: &str = "routing";

type CoordinatorSpawner =
    Box<dyn Fn(PathBuf, String, CoordinatorAction) -> Result<(), String> + Send + Sync>;

/// A request to start or stop the Lab Coordinator supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoordinatorAction {
    Start,
    Stop,
}

/// The four sections of the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    /// Chat with the Lab Coordinator.
    Chat,
    /// The read-only lab Board.
    Board,
    /// The lab state and its usage meters.
    Lab,
    /// The editable role settings.
    Settings,
}

impl Tab {
    /// Every tab, in header order.
    pub const ALL: [Tab; 4] = [Tab::Chat, Tab::Board, Tab::Lab, Tab::Settings];

    /// The plain-text tab title.
    pub fn label(self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Board => "Board",
            Self::Lab => "Lab",
            Self::Settings => "Settings",
        }
    }
}

/// Everything the window reacts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Another tab was picked.
    TabSelected(Tab),
    /// The periodic reload finished with a snapshot.
    SnapshotLoaded(LabSnapshot),
    /// Two seconds passed: reload the snapshot.
    Tick,
    /// The system appearance changed.
    SystemTheme(iced::theme::Mode),
    /// The preferred light/dark mode changed.
    SetTheme(ThemePref),
    /// The color palette changed.
    SetStyle(ThemeStyle),
    /// The read errors banner was closed.
    DismissErrors,
    /// The chat draft was edited.
    DraftChanged(String),
    /// Enter or the Send button was pressed.
    SendPressed,
    /// The background send finished.
    SendFinished(Result<(), String>),
    /// The background ack of the user mailbox finished.
    MailboxAcked,
    /// The Board project filter changed; `None` shows all projects.
    ProjectFilterChanged(Option<String>),
    /// A role's harness was picked.
    SettingsHarnessChanged(FactoryRole, String),
    /// A role's model text changed.
    SettingsModelChanged(FactoryRole, String),
    /// A role's effort was picked.
    SettingsEffortChanged(FactoryRole, String),
    /// The Settings Save button was pressed.
    SettingsSavePressed,
    /// The background role-config save finished, with the configuration it wrote.
    SettingsSaveFinished(Result<(), String>, RoleConfig),
    /// The Settings Revert button was pressed.
    SettingsRevertPressed,
    /// The coordinator Start button was pressed.
    CoordinatorStartPressed,
    /// The coordinator Stop button was pressed.
    CoordinatorStopPressed,
}

/// The window state: the active tab, the latest snapshot and the chat draft.
pub struct App {
    /// Tab shown in the body.
    pub tab: Tab,
    /// Latest state read from the data directory.
    pub snapshot: LabSnapshot,
    /// Whether the read errors banner is open.
    pub errors_visible: bool,
    /// The preferred light/dark mode.
    pub theme_pref: ThemePref,
    /// The selected color palette.
    pub theme_style: ThemeStyle,
    /// Latest appearance reported by the operating system.
    pub system_mode: iced::theme::Mode,
    /// Data directory holding the routing store and the role configuration.
    data_dir: PathBuf,
    /// Chat text being typed.
    pub draft: String,
    /// Whether a send is in flight.
    pub sending: bool,
    /// The last chat error, shown in the existing banner.
    pub send_error: Option<String>,
    /// Selected Board project id; `None` shows every project.
    pub board_filter: Option<String>,
    /// Role configuration with the user's edits on top of the last loaded one.
    pub settings_draft: RoleConfig,
    /// Whether the draft has edits that are not saved yet.
    pub settings_dirty: bool,
    /// The last rejected edit of each role, by role.
    pub settings_errors: BTreeMap<FactoryRole, String>,
    /// The last save error, shown in the existing banner.
    pub settings_error: Option<String>,
    /// Whether the last save succeeded, for the short "Saved" note.
    pub settings_saved: bool,
    /// Whether a save is in flight.
    pub saving_settings: bool,
    /// Error from the latest coordinator command, shown in the Chat status line.
    coordinator_error: Option<String>,
    /// Function used to invoke the coordinator CLI; replaceable in tests.
    coordinator_spawner: CoordinatorSpawner,
}

impl App {
    /// Builds the window state from a first synchronous snapshot load.
    pub fn new() -> (Self, Task<Message>) {
        let data_dir = paths::data_dir();
        let snapshot = LabSnapshot::load(&data_dir, now_secs());
        let mut app = Self::from_snapshot(snapshot);
        app.data_dir = data_dir;
        let appearance = appearance::load(&app.data_dir);
        app.theme_pref = appearance.theme;
        app.theme_style = appearance.style;
        (app, iced::system::theme().map(Message::SystemTheme))
    }

    /// Builds the window state around an already loaded snapshot.
    pub fn from_snapshot(snapshot: LabSnapshot) -> Self {
        Self::from_snapshot_with_spawner(snapshot, Box::new(spawn_coordinator))
    }

    fn from_snapshot_with_spawner(
        snapshot: LabSnapshot,
        coordinator_spawner: CoordinatorSpawner,
    ) -> Self {
        Self {
            tab: Tab::Chat,
            errors_visible: !snapshot.errors.is_empty(),
            theme_pref: ThemePref::System,
            theme_style: ThemeStyle::Default,
            system_mode: iced::theme::Mode::Dark,
            data_dir: paths::data_dir(),
            settings_draft: snapshot.roles.clone(),
            snapshot,
            draft: String::new(),
            sending: false,
            send_error: None,
            board_filter: None,
            settings_dirty: false,
            settings_errors: BTreeMap::new(),
            settings_error: None,
            settings_saved: false,
            saving_settings: false,
            coordinator_error: None,
            coordinator_spawner,
        }
    }

    /// The window title.
    pub fn title(&self) -> String {
        "Dark Factory".to_string()
    }

    /// The theme resolved from the user's preferences and current system appearance.
    pub fn theme(&self) -> Theme {
        jg_ui::resolve_theme(
            self.theme_pref,
            self.theme_style,
            self.system_mode == iced::theme::Mode::Light,
        )
    }

    /// Reloads the snapshot and tracks system appearance changes.
    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::time::every(Duration::from_secs(TICK_SECS)).map(|_| Message::Tick),
            iced::system::theme_changes().map(Message::SystemTheme),
        ])
    }

    /// Applies one message.
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::TabSelected(tab) => {
                self.tab = tab;
                if tab == Tab::Chat {
                    ack_user_mailbox(self.data_dir.clone())
                } else {
                    Task::none()
                }
            }
            Message::Tick => reload_snapshot(self.data_dir.clone()),
            Message::SystemTheme(mode) => {
                self.system_mode = mode;
                Task::none()
            }
            Message::SetTheme(pref) => {
                self.theme_pref = pref;
                self.save_appearance()
            }
            Message::SetStyle(style) => {
                self.theme_style = style;
                self.save_appearance()
            }
            Message::SnapshotLoaded(snapshot) => {
                // A banner the user closed stays closed while the errors are the same.
                if snapshot.errors != self.snapshot.errors {
                    self.errors_visible = !snapshot.errors.is_empty()
                        || self.send_error.is_some()
                        || self.settings_error.is_some();
                }
                // A project that disappeared from the Board no longer filters anything.
                if self
                    .board_filter
                    .as_ref()
                    .is_some_and(|project| !project_choices(&snapshot.tasks).contains(project))
                {
                    self.board_filter = None;
                }
                // Reloads replace a clean draft but never the user's unsaved edits.
                if !self.settings_dirty {
                    self.settings_draft = snapshot.roles.clone();
                    self.settings_errors.clear();
                }
                self.snapshot = snapshot;
                Task::none()
            }
            Message::DismissErrors => {
                self.errors_visible = false;
                Task::none()
            }
            Message::DraftChanged(text) => {
                self.draft = text;
                Task::none()
            }
            Message::SendPressed => self.send_pressed(),
            Message::SendFinished(result) => self.send_finished(result),
            Message::MailboxAcked => reload_snapshot(self.data_dir.clone()),
            Message::ProjectFilterChanged(filter) => {
                self.board_filter = filter;
                Task::none()
            }
            Message::SettingsHarnessChanged(role, value) => {
                self.edit_settings(role, FieldEdit::Harness(value))
            }
            Message::SettingsModelChanged(role, value) => {
                self.edit_settings(role, FieldEdit::Model(value))
            }
            Message::SettingsEffortChanged(role, value) => {
                self.edit_settings(role, FieldEdit::Effort(value))
            }
            Message::SettingsSavePressed => self.save_settings(),
            Message::SettingsSaveFinished(result, saved) => {
                self.settings_save_finished(result, saved)
            }
            Message::SettingsRevertPressed => {
                self.revert_settings();
                Task::none()
            }
            Message::CoordinatorStartPressed => self.coordinator_command(CoordinatorAction::Start),
            Message::CoordinatorStopPressed => self.coordinator_command(CoordinatorAction::Stop),
        }
    }

    /// Saves the currently selected appearance and reports failures through the settings banner.
    fn save_appearance(&mut self) -> Task<Message> {
        let appearance = Appearance {
            theme: self.theme_pref,
            style: self.theme_style,
        };
        match appearance::save(&self.data_dir, appearance) {
            Ok(()) => self.settings_error = None,
            Err(error) => {
                self.settings_error = Some(format!("appearance not saved: {error}"));
                self.errors_visible = true;
            }
        }
        Task::none()
    }

    /// Invokes the supervisor command without taking over chat input or the polling loop.
    fn coordinator_command(&mut self, action: CoordinatorAction) -> Task<Message> {
        self.coordinator_error =
            (self.coordinator_spawner)(self.data_dir.clone(), LAB_ID.to_string(), action).err();
        Task::none()
    }

    /// Validates the draft and starts a background send; a second press is ignored.
    fn send_pressed(&mut self) -> Task<Message> {
        if self.sending {
            return Task::none();
        }

        let body = match validate_draft(&self.draft) {
            Ok(body) => body,
            Err(error) => {
                self.send_error = Some(error.to_string());
                self.errors_visible = true;
                return Task::none();
            }
        };

        self.sending = true;
        self.send_error = None;
        let dir = self.data_dir.join(ROUTING_DIR);
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || deliver_request(&dir, &body))
                    .await
                    .unwrap_or_else(|error| Err(format!("send task failed: {error}")))
            },
            Message::SendFinished,
        )
    }

    /// Clears the draft after a good send or keeps it and shows the error.
    fn send_finished(&mut self, result: Result<(), String>) -> Task<Message> {
        self.sending = false;
        match result {
            Ok(()) => {
                self.draft.clear();
                self.send_error = None;
                reload_snapshot(self.data_dir.clone())
            }
            Err(error) => {
                self.send_error = Some(format!("send failed: {error}"));
                self.errors_visible = true;
                Task::none()
            }
        }
    }

    /// Applies one field edit to the draft; a rejected edit only sets the row error.
    fn edit_settings(&mut self, role: FactoryRole, edit: FieldEdit) -> Task<Message> {
        self.settings_saved = false;
        match apply_edit(&mut self.settings_draft, role, edit) {
            Ok(()) => {
                self.settings_dirty = true;
                self.settings_errors.remove(&role);
            }
            Err(error) => {
                self.settings_errors.insert(role, error.to_string());
            }
        }
        Task::none()
    }

    /// Reverts the draft to the configuration from the last snapshot load.
    fn revert_settings(&mut self) {
        self.settings_draft = self.snapshot.roles.clone();
        self.settings_dirty = false;
        self.settings_errors.clear();
        self.settings_error = None;
        self.settings_saved = false;
        self.errors_visible = self.has_banner();
    }

    /// Validates the draft and saves it on a blocking task; an invalid draft never reaches disk.
    fn save_settings(&mut self) -> Task<Message> {
        if self.saving_settings {
            return Task::none();
        }
        self.settings_saved = false;
        self.settings_error = None;
        if let Err(error) = self.settings_draft.validate() {
            return self.settings_save_failed(format!("invalid configuration: {error}"));
        }

        self.saving_settings = true;
        let path = self.data_dir.join(ROLES_FILE);
        let config = self.settings_draft.clone();
        let saved = config.clone();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || save_roles(&config, &path))
                    .await
                    .unwrap_or_else(|error| Err(format!("save task failed: {error}")))
            },
            move |result| Message::SettingsSaveFinished(result, saved),
        )
    }

    /// Clears dirty after a good save or keeps the draft and shows the error.
    fn settings_save_finished(
        &mut self,
        result: Result<(), String>,
        saved: RoleConfig,
    ) -> Task<Message> {
        self.saving_settings = false;
        match result {
            Ok(()) => {
                self.settings_saved = true;
                self.settings_errors.clear();
                self.settings_error = None;
                self.snapshot.roles = saved.clone();
                // Edits made while the save ran are not on disk, so they stay dirty.
                if self.settings_draft == saved {
                    self.settings_dirty = false;
                }
                Task::none()
            }
            Err(error) => self.settings_save_failed(error),
        }
    }

    /// Records a save error and opens the banner.
    fn settings_save_failed(&mut self, error: String) -> Task<Message> {
        self.settings_error = Some(format!("settings not saved: {error}"));
        self.errors_visible = true;
        Task::none()
    }

    /// Draws the header, the banner, the active tab and the status bar.
    pub fn view(&self) -> Element<'_, Message> {
        let mut layout = column![self.tab_bar()].width(Fill).height(Fill);

        if self.errors_visible && self.has_banner() {
            layout = layout.push(self.error_banner());
        }

        layout
            .push(container(self.tab_content()).width(Fill).height(Fill))
            .push(self.status_bar())
            .into()
    }

    /// The header tab bar.
    fn tab_bar(&self) -> Element<'_, Message> {
        let mut bar = row![].spacing(6).align_y(Alignment::Center);
        for tab in Tab::ALL {
            let active = self.tab == tab;
            let button = button(text(tab.label()))
                .on_press(Message::TabSelected(tab))
                .padding([6, 14]);
            bar = bar.push(if active {
                button.style(button::primary)
            } else {
                button.style(button::secondary)
            });
        }
        container(bar).width(Fill).padding([8, 12]).into()
    }

    /// The banner that lists read, chat and settings errors, with a dismiss button.
    fn error_banner(&self) -> Element<'_, Message> {
        let message = self.banner_message().unwrap_or_default();
        let send_failed = self.send_error.is_some();
        container(
            row![
                text(message).width(Fill),
                button(text("Dismiss"))
                    .on_press(Message::DismissErrors)
                    .style(button::secondary)
                    .padding([2, 10]),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .width(Fill)
        .padding([6, 12])
        .style(move |theme| {
            if send_failed {
                jg_ui::bubble_failed(theme)
            } else {
                jg_ui::banner(jg_ui::ext(theme).danger.base.color)(theme)
            }
        })
        .into()
    }

    /// The body of the active tab.
    fn tab_content(&self) -> Element<'_, Message> {
        match self.tab {
            Tab::Chat => self.chat_tab(),
            Tab::Board => self.board_tab(),
            Tab::Lab => self.lab_tab(),
            Tab::Settings => self.settings_tab(),
        }
    }

    /// The read-only Board: the project filter over ten horizontally scrolling columns.
    fn board_tab(&self) -> Element<'_, Message> {
        let choices = filter_choices(&self.snapshot.tasks);
        let selected = choices
            .iter()
            .find(|choice| choice.project == self.board_filter)
            .or_else(|| choices.first())
            .cloned();
        let picker = pick_list(choices, selected, |choice| {
            Message::ProjectFilterChanged(choice.project)
        })
        .padding([4, 8])
        .text_size(13);

        let board = build_board(
            &self.snapshot.tasks,
            &filter_for(self.board_filter.as_deref()),
        );
        let theme = self.theme();
        let mut columns = row![].spacing(8).height(Fill);
        for column in board.columns {
            columns = columns.push(board_column(column, &theme));
        }

        column![
            row![text("Project").size(13), picker]
                .spacing(8)
                .align_y(Alignment::Center),
            scrollable(columns).horizontal().width(Fill).height(Fill),
        ]
        .spacing(8)
        .width(Fill)
        .height(Fill)
        .into()
    }

    /// The read-only Lab: the big state label over one usage meter per provider.
    fn lab_tab(&self) -> Element<'_, Message> {
        let lab = build_lab_view(self.snapshot.lab_state, &self.snapshot.windows, now_secs());
        let theme = self.theme();
        let state_dot =
            container(space())
                .width(14)
                .height(14)
                .style(jg_ui::dot(style::lab_state_color(
                    &theme,
                    self.snapshot.lab_state,
                )));

        let notice = if matches!(self.snapshot.lab_state, LabState::PausedUntil(_))
            || lab.usage.iter().any(|row| row.level == Level::Hard)
        {
            Some((
                jg_ui::ext(&theme).danger.base.color,
                "Usage limit reached. New work is paused.",
            ))
        } else if self.snapshot.lab_state == LabState::Throttled
            || lab.usage.iter().any(|row| row.level == Level::Soft)
        {
            Some((
                jg_ui::ext(&theme).warning.base.color,
                "Usage is high. Concurrency is reduced.",
            ))
        } else {
            None
        };

        let mut meters = column![].spacing(10).width(Fill);
        if lab.usage.is_empty() {
            meters = meters.push(text("no usage data yet").size(13).style(style::muted_text));
        } else {
            for row in lab.usage {
                meters = meters.push(usage_meter(row, &theme));
            }
        }

        let mut co_coordinators = column![].spacing(4).width(Fill);
        for coordinator in &self.snapshot.co_coordinators {
            if coordinator.coordinator.state == CoordinatorState::Stopped {
                continue;
            }
            let state_color = if coordinator.coordinator.last_error.is_some() {
                style::task_role_state_color(&theme, style::TaskRoleState::Failed)
            } else {
                match coordinator.coordinator.state {
                    CoordinatorState::Running => {
                        style::task_role_state_color(&theme, style::TaskRoleState::Running)
                    }
                    CoordinatorState::Idle | CoordinatorState::Stopped => jg_ui::muted(&theme),
                    CoordinatorState::Backoff => {
                        style::task_role_state_color(&theme, style::TaskRoleState::WaitingForUser)
                    }
                }
            };
            let state = match coordinator.coordinator.state {
                CoordinatorState::Running => "running",
                CoordinatorState::Idle => "idle",
                CoordinatorState::Backoff => "backing off",
                CoordinatorState::Stopped => "stopped",
            };
            let mut line = format!("Co-coordinator {}: {state}", coordinator.project);
            if let Some(error) = coordinator.coordinator.last_error.as_deref() {
                line.push_str(" — ");
                line.push_str(error);
            }
            co_coordinators = co_coordinators.push(text(line).size(12).style(move |_| {
                iced::widget::text::Style {
                    color: Some(state_color),
                }
            }));
        }

        let mut content = column![
            row![state_dot, text(lab.state_label).size(26)]
                .spacing(10)
                .align_y(Alignment::Center),
        ]
        .spacing(18)
        .width(Fill)
        .height(Fill)
        .padding(24);
        if let Some((accent, message)) = notice {
            content = content.push(
                container(text(message))
                    .width(Fill)
                    .padding([8, 12])
                    .style(jg_ui::banner(accent)),
            );
        }
        content.push(meters).push(co_coordinators).into()
    }

    /// The Settings tab: one editable row per role over the Save and Revert actions.
    fn settings_tab(&self) -> Element<'_, Message> {
        let form = SettingsForm::from_config(&self.settings_draft);
        let mut rows = column![].spacing(12).width(Fill);
        for row in form.rows {
            rows = rows.push(self.settings_row(row));
        }

        let theme_button = |label, pref| {
            button(text(label))
                .on_press(Message::SetTheme(pref))
                .style(jg_ui::segment_button(self.theme_pref == pref))
                .padding([4, 12])
        };
        let style_button = |label, selected| {
            button(text(label))
                .on_press(Message::SetStyle(selected))
                .style(jg_ui::segment_button(self.theme_style == selected))
                .padding([4, 12])
        };
        let theme_control = container(
            row![
                theme_button("System", ThemePref::System),
                theme_button("Dark", ThemePref::Dark),
                theme_button("Light", ThemePref::Light),
            ]
            .spacing(2)
            .align_y(Alignment::Center),
        )
        .padding(3)
        .style(jg_ui::segmented_container);
        let style_control = container(
            row![
                style_button("Default", ThemeStyle::Default),
                style_button("Violet", ThemeStyle::Violet),
                style_button("Tokyo Night", ThemeStyle::TokyoNight),
                style_button("Dracula", ThemeStyle::Dracula),
            ]
            .spacing(2)
            .align_y(Alignment::Center),
        )
        .padding(3)
        .style(jg_ui::segmented_container);
        let appearance = container(
            column![
                text("Appearance").size(16),
                row![text("Theme").width(100), theme_control]
                    .spacing(12)
                    .align_y(Alignment::Center),
                row![text("Style").width(100), style_control]
                    .spacing(12)
                    .align_y(Alignment::Center),
            ]
            .spacing(12)
            .width(Fill),
        )
        .padding(16)
        .width(Fill)
        .style(jg_ui::card(jg_ui::ext(&self.theme()).primary.base.color));
        let role_settings = container(
            column![text("Role settings").size(16), rows]
                .spacing(12)
                .width(Fill),
        )
        .padding(16)
        .width(Fill)
        .style(jg_ui::card(jg_ui::ext(&self.theme()).primary.base.color));

        let save = button(text("Save"))
            .style(jg_ui::primary_button)
            .padding([6, 16]);
        let save = if self.saving_settings {
            save
        } else {
            save.on_press(Message::SettingsSavePressed)
        };
        let revert = button(text("Revert"))
            .on_press(Message::SettingsRevertPressed)
            .style(jg_ui::ghost)
            .padding([6, 16]);

        let mut actions = row![save, revert].spacing(8).align_y(Alignment::Center);
        if self.settings_saved {
            actions = actions.push(text("Saved").size(12).style(style::success_text));
        }

        column![
            scrollable(column![appearance, role_settings].spacing(16).width(Fill)).height(Fill),
            actions
        ]
        .spacing(12)
        .width(Fill)
        .height(Fill)
        .padding(24)
        .into()
    }

    /// One role's settings row, with its rejected-edit error underneath.
    fn settings_row(&self, row: SettingsRow) -> Element<'_, Message> {
        let role = row.role;
        let model = self
            .settings_draft
            .get(role)
            .map_or("", |settings| settings.model.as_str());

        let harness = pick_list(
            harness_options(),
            option_for(harness_options(), &row.harness),
            move |value: &'static str| Message::SettingsHarnessChanged(role, value.to_string()),
        )
        .padding([4, 8])
        .text_size(13)
        .style(jg_ui::workspace_selector)
        .menu_style(jg_ui::workspace_menu);

        let effort = pick_list(
            effort_options(),
            option_for(effort_options(), &row.effort),
            move |value: &'static str| Message::SettingsEffortChanged(role, value.to_string()),
        )
        .padding([4, 8])
        .text_size(13)
        .style(jg_ui::workspace_selector)
        .menu_style(jg_ui::workspace_menu);

        let model_input = text_input("model", model)
            .on_input(move |value| Message::SettingsModelChanged(role, value))
            .padding([4, 8])
            .style(jg_ui::input)
            .width(Fill);

        let fields = row![
            text(row.label).size(13).width(120),
            harness,
            model_input,
            effort,
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let mut body = column![fields].spacing(3).width(Fill);
        if let Some(error) = self.settings_errors.get(&role) {
            body = body.push(text(error).size(11).style(style::error_text));
        }
        body.into()
    }

    /// The chat with the Lab Coordinator: the message log over the draft input.
    fn chat_tab(&self) -> Element<'_, Message> {
        column![
            self.coordinator_control(),
            self.chat_log(),
            self.chat_input()
        ]
        .spacing(8)
        .width(Fill)
        .height(Fill)
        .into()
    }

    /// The coordinator state and the matching Start or Stop action.
    fn coordinator_control(&self) -> Element<'_, Message> {
        let failed =
            self.coordinator_error.is_some() || self.snapshot.coordinator.last_error.is_some();
        let theme = self.theme();
        let color = if failed {
            style::task_role_state_color(&theme, style::TaskRoleState::Failed)
        } else {
            match self.snapshot.coordinator.state {
                CoordinatorState::Running => {
                    style::task_role_state_color(&theme, style::TaskRoleState::Running)
                }
                CoordinatorState::Backoff => {
                    style::task_role_state_color(&theme, style::TaskRoleState::WaitingForUser)
                }
                CoordinatorState::Idle | CoordinatorState::Stopped => jg_ui::muted(&theme),
            }
        };
        let status = text(self.coordinator_status_line())
            .style(move |_| iced::widget::text::Style { color: Some(color) });

        let action = match self.coordinator_control_action() {
            CoordinatorAction::Start => button(text("Start"))
                .on_press(Message::CoordinatorStartPressed)
                .style(button::primary),
            CoordinatorAction::Stop => button(text("Stop"))
                .on_press(Message::CoordinatorStopPressed)
                .style(button::secondary),
        };

        row![status.width(Fill), action.padding([6, 14])]
            .spacing(8)
            .align_y(Alignment::Center)
            .width(Fill)
            .into()
    }

    /// A stopped coordinator has no fresh heartbeat, so it can be started again.
    fn coordinator_control_action(&self) -> CoordinatorAction {
        match self.snapshot.coordinator.state {
            CoordinatorState::Running | CoordinatorState::Idle | CoordinatorState::Backoff => {
                CoordinatorAction::Stop
            }
            CoordinatorState::Stopped => CoordinatorAction::Start,
        }
    }

    /// Status text combines the persisted supervisor error and any command error.
    fn coordinator_status_line(&self) -> String {
        let state = match self.snapshot.coordinator.state {
            CoordinatorState::Stopped => "stopped",
            CoordinatorState::Running => "running",
            CoordinatorState::Idle => "idle",
            CoordinatorState::Backoff => "backing off",
        };
        let mut line = format!("Coordinator: {state}");
        if let Some(error) = self.snapshot.coordinator.last_error.as_deref() {
            line.push_str(" — ");
            line.push_str(error);
        }
        if let Some(error) = self.coordinator_error.as_deref() {
            line.push_str(" — ");
            line.push_str(error);
        }
        line
    }

    /// The scrollable message log.
    fn chat_log(&self) -> Element<'_, Message> {
        let mut log = column![].spacing(6).width(Fill);
        if self.snapshot.chat.lines.is_empty() {
            let hint = if self.snapshot.coordinator.state == CoordinatorState::Stopped {
                "Messages will wait until the Lab Coordinator is started."
            } else {
                "No messages yet. Say hello to the Lab Coordinator."
            };
            log = log.push(text(hint));
        } else {
            for line in &self.snapshot.chat.lines {
                log = log.push(chat_line(line));
            }
        }
        scrollable(log).height(Fill).anchor_bottom().into()
    }

    /// The single-line draft input and its Send button; Enter also sends.
    fn chat_input(&self) -> Element<'_, Message> {
        let input = text_input("Message the Lab Coordinator…", &self.draft)
            .on_input(Message::DraftChanged)
            .on_submit(Message::SendPressed)
            .padding(8)
            .width(Fill)
            .style(jg_ui::input);

        let send = button(text("Send")).padding([8, 16]).style(button::primary);
        let send = if self.sending {
            send
        } else {
            send.on_press(Message::SendPressed)
        };

        container(row![input, send].spacing(8).align_y(Alignment::Center))
            .width(Fill)
            .style(jg_ui::composer_box)
            .into()
    }

    /// The persistent status bar: lab state, the first provider's usage and chat unread.
    fn status_bar(&self) -> Element<'_, Message> {
        let lab = build_lab_view(self.snapshot.lab_state, &self.snapshot.windows, now_secs());
        let unread = self.snapshot.chat.unread_from_coordinator;
        container(text(status_text(&lab, unread)))
            .width(Fill)
            .padding([6, 12])
            .style(container::dark)
            .into()
    }

    /// Whether the banner has something to show.
    fn has_banner(&self) -> bool {
        self.banner_message().is_some()
    }

    /// The banner text: read errors plus the last chat or settings error.
    fn banner_message(&self) -> Option<String> {
        let mut messages = self.snapshot.errors.clone();
        if let Some(error) = &self.send_error {
            messages.push(error.clone());
        }
        if let Some(error) = &self.settings_error {
            messages.push(error.clone());
        }
        if messages.is_empty() {
            None
        } else {
            Some(messages.join("; "))
        }
    }
}

/// One chat message bubble, with the user's lines on the right.
fn chat_line(line: &ChatLine) -> Element<'_, Message> {
    let speaker = if line.from_user {
        "You"
    } else {
        "Lab Coordinator"
    };
    let bubble = container(
        column![
            text(speaker).size(11),
            text(&line.subject).size(14),
            text(&line.body),
        ]
        .spacing(2),
    )
    .padding(8)
    .max_width(640.0)
    .style(if line.from_user {
        jg_ui::bubble_user
    } else {
        jg_ui::bubble_agent
    });

    if line.from_user {
        container(bubble).width(Fill).align_x(Alignment::End).into()
    } else {
        container(bubble)
            .width(Fill)
            .align_x(Alignment::Start)
            .into()
    }
}

/// One provider's usage meter: name, progress bar, amounts, percent and reset label.
fn usage_meter(row: UsageRow, theme: &Theme) -> Element<'static, Message> {
    let UsageRow {
        provider,
        percent,
        used,
        limit,
        resets_label,
        level,
    } = row;

    let bar = progress_bar(0.0..=100.0, bar_percent(percent))
        .length(Fill)
        .girth(10)
        .style(style::level_progress_bar(level));

    let details = row![
        text(format!("{used} / {limit}"))
            .size(12)
            .style(style::muted_text),
        text(format!("{percent}%"))
            .size(12)
            .style(style::level_text(level)),
        text(resets_label).size(12).style(style::muted_text),
    ]
    .spacing(12)
    .align_y(Alignment::Center);

    container(
        column![text(provider).size(14), bar, details]
            .spacing(5)
            .width(Fill),
    )
    .width(Fill)
    .padding([8, 10])
    .style(jg_ui::card(style::level_color(theme, level)))
    .into()
}

/// The progress bar value, clamped to its 0-100 range.
fn bar_percent(percent: u64) -> f32 {
    percent.min(100) as f32
}

/// The static option matching `value`, for a pick list's selected value.
fn option_for(options: &'static [&'static str], value: &str) -> Option<&'static str> {
    options.iter().copied().find(|option| *option == value)
}

/// One Board column: its title and count over the scrollable card list.
fn board_column(column: ColumnView, theme: &Theme) -> Element<'static, Message> {
    let count = column.cards.len();
    let header = row![
        text(column.title).size(13),
        text(count.to_string()).size(12).style(style::muted_text),
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    let mut cards = column![].spacing(6).width(Fill);
    if column.cards.is_empty() {
        cards = cards.push(text("empty").size(12).style(style::muted_text));
    } else {
        for card in column.cards {
            let state = board_task_state(column.column, card.status_note.is_some());
            cards = cards.push(board_card(card, state, theme));
        }
    }

    container(
        column![header, scrollable(cards).height(Fill)]
            .spacing(8)
            .height(Fill),
    )
    .width(220)
    .height(Fill)
    .padding(8)
    .style(jg_ui::board_column)
    .into()
}

/// One read-only card: project badge, key, wrapped plain title and status note.
fn board_card(
    card: CardView,
    state: style::TaskRoleState,
    theme: &Theme,
) -> Element<'static, Message> {
    let accent = style::task_role_state_color(theme, state);
    let state_label = match state {
        style::TaskRoleState::Running => "running",
        style::TaskRoleState::WaitingForUser => "waiting for user",
        style::TaskRoleState::Failed => "failed",
        style::TaskRoleState::Done => "done",
        style::TaskRoleState::Queued => "queued",
    };
    let state_badge = container(text(state_label).size(10))
        .padding([1, 6])
        .style(jg_ui::badge(accent));
    let badge = container(text(card.project_badge).size(10))
        .padding([1, 6])
        .style(jg_ui::pill);

    let mut body = column![
        text(card.title)
            .size(13)
            .width(Fill)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
    ]
    .spacing(3)
    .width(Fill);
    if let Some(note) = card.status_note {
        body = body.push(text(note).size(11).style(style::muted_text));
    }

    container(
        column![
            row![state_badge, badge, text(card.key).size(11)]
                .spacing(6)
                .align_y(Alignment::Center),
            body,
        ]
        .spacing(4)
        .width(Fill),
    )
    .width(Fill)
    .padding(8)
    .style(jg_ui::card(accent))
    .into()
}

/// Maps Board columns and failed outcomes to the small state language used by their cards.
fn board_task_state(column: Column, failed: bool) -> style::TaskRoleState {
    if failed {
        return style::TaskRoleState::Failed;
    }

    match column {
        Column::Backlog | Column::OnQueue => style::TaskRoleState::Queued,
        Column::Working | Column::Testing => style::TaskRoleState::Running,
        Column::Review | Column::ToCommit | Column::ToPush | Column::ToOpenPr | Column::ToMerge => {
            style::TaskRoleState::WaitingForUser
        }
        Column::Done => style::TaskRoleState::Done,
    }
}

/// Reloads the snapshot away from the UI thread.
fn reload_snapshot(data_dir: PathBuf) -> Task<Message> {
    let now = now_secs();
    Task::perform(
        async move {
            tokio::task::spawn_blocking(move || LabSnapshot::load(&data_dir, now))
                .await
                .unwrap_or_else(|error| LabSnapshot {
                    errors: vec![format!("snapshot task failed: {error}")],
                    ..LabSnapshot::default()
                })
        },
        Message::SnapshotLoaded,
    )
}

/// Writes the role configuration to `path`; the save runs on a blocking task.
fn save_roles(config: &RoleConfig, path: &Path) -> Result<(), String> {
    config.save(path).map_err(|error| error.to_string())
}

/// Sends one validated request to the Lab Coordinator through the router.
fn deliver_request(routing_dir: &Path, body: &str) -> Result<(), String> {
    let router = Router::open(routing_dir).map_err(|error| error.to_string())?;
    router
        .send(OutgoingMessage {
            from: Role::User,
            to: Role::LabCoordinator,
            kind: MessageKind::UserRequest,
            project: None,
            subject: subject_from(body),
            body: body.to_string(),
            wakes: false,
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Acking the user's mailbox happens away from the UI thread.
fn ack_user_mailbox(data_dir: PathBuf) -> Task<Message> {
    let dir = data_dir.join(ROUTING_DIR);
    Task::perform(
        async move {
            tokio::task::spawn_blocking(move || ack_user(&dir))
                .await
                .unwrap_or_else(|error| Err(format!("ack task failed: {error}")))
        },
        |_| Message::MailboxAcked,
    )
}

/// Marks every message in the user's mailbox as read.
fn ack_user(routing_dir: &Path) -> Result<(), String> {
    Router::open(routing_dir)
        .and_then(|router| router.ack(&Role::User))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// The status bar text for `lab` and the chat unread count.
fn status_text(lab: &LabView, unread: usize) -> String {
    let base = match lab.usage.first() {
        Some(row) => format!("{} · {} {}%", lab.state_label, row.provider, row.percent),
        None => lab.state_label.clone(),
    };
    if unread > 0 {
        format!("{base} · {unread} unread")
    } else {
        base
    }
}

/// Starts or asks the coordinator supervisor to stop via the adjacent CLI or PATH.
fn spawn_coordinator(
    state_dir: PathBuf,
    lab: String,
    action: CoordinatorAction,
) -> Result<(), String> {
    let binary = jg_factory_binary();
    let command_name = match action {
        CoordinatorAction::Start => "run",
        CoordinatorAction::Stop => "stop",
    };
    let mut command = Command::new(binary);
    command
        .arg("coordinator")
        .arg(command_name)
        .arg("--state-dir")
        .arg(state_dir)
        .arg("--lab")
        .arg(lab);

    match action {
        CoordinatorAction::Start => command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Could not start coordinator: {error}")),
        CoordinatorAction::Stop => {
            let output = command
                .output()
                .map_err(|error| format!("Could not stop coordinator: {error}"))?;
            if output.status.success() {
                Ok(())
            } else {
                let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
                if detail.is_empty() {
                    Err(format!(
                        "Coordinator stop command failed with {}",
                        output.status
                    ))
                } else {
                    Err(detail)
                }
            }
        }
    }
}

/// Finds `jg-factory` beside the desktop executable, then falls back to PATH lookup.
fn jg_factory_binary() -> PathBuf {
    let name = if cfg!(windows) {
        "jg-factory.exe"
    } else {
        "jg-factory"
    };
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(|parent| parent.join(name)));
    if let Some(sibling) = sibling.filter(|path| path.is_file()) {
        sibling
    } else {
        PathBuf::from(name)
    }
}

/// The current unix time in seconds.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use jgf_core::ProjectId;
    use jgf_hub::{
        MessageKind, OutgoingMessage, Role, Router,
        board::{Column, Task},
        usage::{LabState, Level, Provider, Window},
    };

    use super::*;

    fn task(key: &str) -> Task {
        task_in(key, "alpha", Column::Backlog)
    }

    fn task_in(key: &str, project_id: &str, status: Column) -> Task {
        Task {
            key: key.to_string(),
            project: ProjectId::parse(project_id).expect("project id"),
            title: format!("title of {key}"),
            description: String::new(),
            details: None,
            status,
            test_result: None,
            review_result: None,
            created_ms: 0,
            updated_ms: 0,
        }
    }

    fn snapshot_with_error() -> LabSnapshot {
        LabSnapshot {
            errors: vec!["board.json: not valid JSON".to_string()],
            ..LabSnapshot::default()
        }
    }

    #[test]
    fn tab_selection_updates_the_active_tab() {
        let mut app = App::from_snapshot(LabSnapshot::default());
        assert_eq!(app.tab, Tab::Chat);

        let _ = app.update(Message::TabSelected(Tab::Board));
        assert_eq!(app.tab, Tab::Board);

        let _ = app.update(Message::TabSelected(Tab::Settings));
        assert_eq!(app.tab, Tab::Settings);

        assert_eq!(
            Tab::ALL.map(Tab::label),
            ["Chat", "Board", "Lab", "Settings"]
        );
    }

    #[test]
    fn system_theme_message_updates_the_system_mode() {
        let mut app = App::from_snapshot(LabSnapshot::default());
        assert!(jg_ui::ext(&app.theme()).is_dark);

        let _ = app.update(Message::SystemTheme(iced::theme::Mode::Light));

        assert_eq!(app.system_mode, iced::theme::Mode::Light);
        assert!(!jg_ui::ext(&app.theme()).is_dark);
    }

    #[test]
    fn settings_view_builds_for_each_theme_and_style() {
        let mut app = App::from_snapshot(LabSnapshot::default());
        app.tab = Tab::Settings;

        for theme in [ThemePref::System, ThemePref::Dark, ThemePref::Light] {
            for style in [
                ThemeStyle::Default,
                ThemeStyle::Violet,
                ThemeStyle::TokyoNight,
                ThemeStyle::Dracula,
            ] {
                app.theme_pref = theme;
                app.theme_style = style;
                let _ = app.view();
            }
        }
    }

    #[test]
    fn theme_and_style_messages_update_and_persist_appearance() {
        let directory = tempfile::tempdir().expect("temporary data directory");
        let mut app = temp_app(directory.path());

        let _ = app.update(Message::SetTheme(ThemePref::Light));
        assert_eq!(app.theme_pref, ThemePref::Light);
        let _ = app.update(Message::SetStyle(ThemeStyle::Dracula));

        assert_eq!(app.theme_style, ThemeStyle::Dracula);
        assert_eq!(
            appearance::load(directory.path()),
            Appearance {
                theme: ThemePref::Light,
                style: ThemeStyle::Dracula,
            }
        );
        assert!(app.settings_error.is_none());
    }

    #[test]
    fn appearance_save_failure_uses_the_settings_error_banner() {
        let directory = tempfile::tempdir().expect("temporary data directory");
        let data_path = directory.path().join("not-a-directory");
        std::fs::write(&data_path, "file").expect("create non-directory path");
        let mut app = temp_app(&data_path);

        let _ = app.update(Message::SetTheme(ThemePref::Light));

        assert_eq!(app.theme_pref, ThemePref::Light);
        assert!(app.errors_visible);
        assert!(
            app.settings_error
                .as_deref()
                .is_some_and(|error| error.contains("appearance not saved"))
        );
    }

    #[test]
    fn coordinator_start_and_stop_messages_use_the_injected_spawner() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded_calls = Arc::clone(&calls);
        let mut app = App::from_snapshot_with_spawner(
            LabSnapshot::default(),
            Box::new(move |state_dir, lab, action| {
                recorded_calls
                    .lock()
                    .expect("spawner calls lock")
                    .push((state_dir, lab, action));
                Ok(())
            }),
        );
        let dir = tempfile::tempdir().expect("temp dir");
        app.data_dir = dir.path().to_path_buf();

        let _ = app.update(Message::CoordinatorStartPressed);
        let _ = app.update(Message::CoordinatorStopPressed);

        assert_eq!(
            *calls.lock().expect("spawner calls lock"),
            [
                (
                    dir.path().to_path_buf(),
                    LAB_ID.to_string(),
                    CoordinatorAction::Start
                ),
                (
                    dir.path().to_path_buf(),
                    LAB_ID.to_string(),
                    CoordinatorAction::Stop
                ),
            ]
        );
        assert_eq!(app.coordinator_error, None);
    }

    #[test]
    fn coordinator_spawn_failure_is_shown_in_the_status_line() {
        let mut app = App::from_snapshot_with_spawner(
            LabSnapshot::default(),
            Box::new(|_, _, _| Err("spawn denied".to_string())),
        );

        let _ = app.update(Message::CoordinatorStartPressed);

        assert!(
            app.coordinator_status_line()
                .contains("Coordinator: stopped")
        );
        assert!(app.coordinator_status_line().contains("spawn denied"));
    }

    #[test]
    fn stopped_coordinator_offers_start_control() {
        let app = App::from_snapshot(LabSnapshot::default());

        assert_eq!(app.coordinator_control_action(), CoordinatorAction::Start);
    }

    #[test]
    fn tick_result_replaces_the_snapshot() {
        let mut app = App::from_snapshot(LabSnapshot::default());
        let snapshot = LabSnapshot {
            tasks: vec![task("ALPHA-1")],
            ..LabSnapshot::default()
        };

        let _ = app.update(Message::SnapshotLoaded(snapshot));

        assert_eq!(app.snapshot.tasks.len(), 1);
        assert_eq!(app.snapshot.tasks[0].key, "ALPHA-1");
    }

    #[test]
    fn the_project_filter_narrows_the_board() {
        let mut app = App::from_snapshot(LabSnapshot {
            tasks: vec![
                task_in("alpha-1", "alpha", Column::Backlog),
                task_in("beta-1", "beta", Column::Working),
            ],
            ..LabSnapshot::default()
        });

        let _ = app.update(Message::ProjectFilterChanged(Some("beta".to_string())));

        assert_eq!(app.board_filter.as_deref(), Some("beta"));
        let board = build_board(
            &app.snapshot.tasks,
            &filter_for(app.board_filter.as_deref()),
        );
        let keys: Vec<&str> = board
            .columns
            .iter()
            .flat_map(|column| column.cards.iter().map(|card| card.key.as_str()))
            .collect();
        assert_eq!(keys, ["beta-1"]);
    }

    #[test]
    fn a_filter_for_a_vanished_project_resets_on_reload() {
        let mut app = App::from_snapshot(LabSnapshot {
            tasks: vec![
                task_in("alpha-1", "alpha", Column::Backlog),
                task_in("beta-1", "beta", Column::Working),
            ],
            ..LabSnapshot::default()
        });
        let _ = app.update(Message::ProjectFilterChanged(Some("beta".to_string())));

        let _ = app.update(Message::SnapshotLoaded(LabSnapshot {
            tasks: vec![task_in("alpha-1", "alpha", Column::Backlog)],
            ..LabSnapshot::default()
        }));

        assert_eq!(app.board_filter, None);
    }

    #[test]
    fn a_filter_survives_a_reload_when_the_project_still_exists() {
        let mut app = App::from_snapshot(LabSnapshot {
            tasks: vec![task_in("beta-1", "beta", Column::Working)],
            ..LabSnapshot::default()
        });
        let _ = app.update(Message::ProjectFilterChanged(Some("beta".to_string())));

        let _ = app.update(Message::SnapshotLoaded(LabSnapshot {
            tasks: vec![
                task_in("alpha-1", "alpha", Column::Backlog),
                task_in("beta-2", "beta", Column::Review),
            ],
            ..LabSnapshot::default()
        }));

        assert_eq!(app.board_filter.as_deref(), Some("beta"));
    }

    #[test]
    fn errors_show_in_a_dismissible_banner() {
        let mut app = App::from_snapshot(snapshot_with_error());
        assert!(app.errors_visible);

        let _ = app.update(Message::DismissErrors);
        assert!(!app.errors_visible);

        let _ = app.update(Message::SnapshotLoaded(snapshot_with_error()));
        assert!(!app.errors_visible);

        let _ = app.update(Message::SnapshotLoaded(LabSnapshot::default()));
        assert!(!app.errors_visible);
    }

    #[test]
    fn a_snapshot_with_errors_opens_the_banner_again() {
        let mut app = App::from_snapshot(LabSnapshot::default());
        assert!(!app.errors_visible);

        let _ = app.update(Message::SnapshotLoaded(snapshot_with_error()));
        assert!(app.errors_visible);
        assert_eq!(app.snapshot.errors.len(), 1);
    }

    /// An app whose routing store lives under `dir`.
    fn temp_app(dir: &Path) -> App {
        let mut app = App::from_snapshot(LabSnapshot::default());
        app.data_dir = dir.to_path_buf();
        app
    }

    #[test]
    fn a_draft_change_updates_the_state() {
        let mut app = App::from_snapshot(LabSnapshot::default());

        let _ = app.update(Message::DraftChanged("hello".to_string()));

        assert_eq!(app.draft, "hello");
    }

    #[test]
    fn an_invalid_draft_never_starts_a_send() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut app = temp_app(dir.path());
        app.draft = "   ".to_string();

        let task = app.update(Message::SendPressed);

        assert_eq!(task.units(), 0);
        assert!(!app.sending);
        assert_eq!(app.draft, "   ");
        assert!(app.send_error.is_some());
        assert!(app.errors_visible);
    }

    #[test]
    fn send_success_sends_and_clears_the_draft() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut app = temp_app(dir.path());
        app.draft = "  hello lab \nsecond line  ".to_string();

        let task = app.update(Message::SendPressed);
        assert_eq!(task.units(), 1);
        assert!(app.sending);

        let routing = dir.path().join(ROUTING_DIR);
        let body = validate_draft(&app.draft).expect("valid draft");
        deliver_request(&routing, &body).expect("deliver");

        let _ = app.update(Message::SendFinished(Ok(())));

        assert!(!app.sending);
        assert!(app.draft.is_empty());
        assert!(app.send_error.is_none());

        let router = Router::open(&routing).expect("open router");
        let inbox = router
            .inbox(&Role::LabCoordinator)
            .expect("lab coordinator inbox");
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].kind, MessageKind::UserRequest);
        assert_eq!(inbox[0].subject, "hello lab");
        assert_eq!(inbox[0].body, "hello lab \nsecond line");
    }

    #[test]
    fn a_send_failure_keeps_the_draft_and_shows_the_banner() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut app = temp_app(dir.path());
        app.draft = "hello".to_string();

        let _ = app.update(Message::SendPressed);
        let _ = app.update(Message::SendFinished(Err(
            "mailbox is read-only".to_string()
        )));

        assert!(!app.sending);
        assert_eq!(app.draft, "hello");
        assert!(app.errors_visible);
        assert_eq!(
            app.send_error.as_deref(),
            Some("send failed: mailbox is read-only")
        );
        assert!(
            app.banner_message()
                .is_some_and(|message| message.contains("mailbox is read-only"))
        );
    }

    #[test]
    fn a_second_press_while_sending_is_ignored() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut app = temp_app(dir.path());
        app.draft = "hello".to_string();

        let first = app.update(Message::SendPressed);
        assert!(first.units() > 0);
        assert!(app.sending);

        let second = app.update(Message::SendPressed);

        assert_eq!(second.units(), 0);
        assert!(app.sending);
        assert_eq!(app.draft, "hello");
    }

    #[test]
    fn entering_chat_acks_the_user_mailbox() {
        let dir = tempfile::tempdir().expect("temp dir");
        let routing = dir.path().join(ROUTING_DIR);
        let router = Router::open(&routing).expect("open router");
        router
            .send(OutgoingMessage::new(
                Role::LabCoordinator,
                Role::User,
                MessageKind::Report,
                "progress",
                "working on it",
            ))
            .expect("report");
        assert_eq!(router.inbox(&Role::User).expect("user inbox").len(), 1);

        let mut app = temp_app(dir.path());
        app.tab = Tab::Board;
        let task = app.update(Message::TabSelected(Tab::Chat));

        assert_eq!(app.tab, Tab::Chat);
        assert!(task.units() > 0);

        ack_user(&routing).expect("ack user mailbox");
        assert!(router.inbox(&Role::User).expect("user inbox").is_empty());
    }

    #[test]
    fn lab_rows_cover_every_level_and_the_no_data_case() {
        let windows = [
            (
                Provider::parse("claude").expect("provider"),
                Window::new(10, 100, 10_000).expect("window"),
            ),
            (
                Provider::parse("opencode").expect("provider"),
                Window::new(80, 100, 10_000).expect("window"),
            ),
            (
                Provider::parse("gemini").expect("provider"),
                Window::new(95, 100, 10_000).expect("window"),
            ),
        ];
        let view = build_lab_view(LabState::Running, &windows, 0);
        let levels: Vec<Level> = view.usage.iter().map(|row| row.level).collect();
        assert_eq!(levels, [Level::Normal, Level::Soft, Level::Hard]);

        let empty = build_lab_view(LabState::Idle, &[], 0);
        assert!(empty.usage.is_empty());
        assert_eq!(empty.state_label, "Idle");
        assert_eq!(bar_percent(120), 100.0);
    }

    #[test]
    fn editing_a_model_marks_the_settings_draft_dirty() {
        let mut app = App::from_snapshot(LabSnapshot::default());
        let baseline = app.settings_draft.clone();

        let _ = app.update(Message::SettingsModelChanged(
            FactoryRole::Coder,
            "provider/model-1".to_string(),
        ));

        assert!(app.settings_dirty);
        assert!(app.settings_errors.is_empty());
        assert_ne!(app.settings_draft, baseline);
        assert_eq!(
            app.settings_draft
                .get(FactoryRole::Coder)
                .expect("coder settings")
                .model,
            "provider/model-1"
        );
    }

    #[test]
    fn a_dirty_settings_draft_survives_a_tick() {
        let mut app = App::from_snapshot(LabSnapshot::default());
        let _ = app.update(Message::SettingsModelChanged(
            FactoryRole::Coder,
            "provider/model-1".to_string(),
        ));
        let draft = app.settings_draft.clone();

        let _ = app.update(Message::Tick);
        let _ = app.update(Message::SnapshotLoaded(LabSnapshot {
            roles: RoleConfig::defaults(),
            ..LabSnapshot::default()
        }));

        assert!(app.settings_dirty);
        assert_eq!(app.settings_draft, draft);
    }

    #[test]
    fn a_clean_settings_draft_follows_a_reload() {
        let mut app = App::from_snapshot(LabSnapshot::default());
        let mut roles = RoleConfig::defaults();
        apply_edit(
            &mut roles,
            FactoryRole::Coder,
            FieldEdit::Model("provider/model-9".to_string()),
        )
        .expect("valid edit");

        let _ = app.update(Message::SnapshotLoaded(LabSnapshot {
            roles: roles.clone(),
            ..LabSnapshot::default()
        }));

        assert!(!app.settings_dirty);
        assert_eq!(app.settings_draft, roles);
    }

    #[test]
    fn an_invalid_edit_keeps_the_draft_and_sets_the_row_error() {
        let mut app = App::from_snapshot(LabSnapshot::default());
        let before = app.settings_draft.clone();

        let _ = app.update(Message::SettingsModelChanged(
            FactoryRole::Coder,
            String::new(),
        ));

        assert_eq!(app.settings_draft, before);
        assert!(!app.settings_dirty);
        let error = app
            .settings_errors
            .get(&FactoryRole::Coder)
            .expect("row error");
        assert!(error.contains("invalid model"), "{error}");
    }

    #[test]
    fn revert_restores_the_loaded_configuration() {
        let mut app = App::from_snapshot(LabSnapshot::default());
        let _ = app.update(Message::SettingsModelChanged(
            FactoryRole::Coder,
            "provider/model-1".to_string(),
        ));
        let _ = app.update(Message::SettingsEffortChanged(
            FactoryRole::Coder,
            "max".to_string(),
        ));

        let _ = app.update(Message::SettingsRevertPressed);

        assert!(!app.settings_dirty);
        assert!(app.settings_errors.is_empty());
        assert_eq!(app.settings_draft, app.snapshot.roles);
    }

    #[test]
    fn a_successful_save_writes_the_file_and_clears_dirty() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut app = temp_app(dir.path());
        let _ = app.update(Message::SettingsModelChanged(
            FactoryRole::Coder,
            "provider/model-1".to_string(),
        ));
        let draft = app.settings_draft.clone();
        let path = dir.path().join(ROLES_FILE);

        let task = app.update(Message::SettingsSavePressed);
        assert_eq!(task.units(), 1);
        assert!(app.saving_settings);

        save_roles(&draft, &path).expect("save roles");
        let _ = app.update(Message::SettingsSaveFinished(Ok(()), draft.clone()));

        assert!(!app.saving_settings);
        assert!(!app.settings_dirty);
        assert!(app.settings_saved);
        assert_eq!(RoleConfig::load(&path).expect("load roles"), draft);
    }

    #[test]
    fn an_edit_during_a_save_stays_dirty_and_survives_a_tick() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut app = temp_app(dir.path());
        let _ = app.update(Message::SettingsModelChanged(
            FactoryRole::Coder,
            "provider/model-1".to_string(),
        ));
        let saved = app.settings_draft.clone();

        let _ = app.update(Message::SettingsSavePressed);
        assert!(app.saving_settings);

        // The user keeps editing while the save runs.
        let _ = app.update(Message::SettingsModelChanged(
            FactoryRole::Coder,
            "provider/model-2".to_string(),
        ));
        let draft = app.settings_draft.clone();
        assert_ne!(draft, saved);

        let path = dir.path().join(ROLES_FILE);
        save_roles(&saved, &path).expect("save roles");
        let _ = app.update(Message::SettingsSaveFinished(Ok(()), saved.clone()));

        assert!(!app.saving_settings);
        assert!(app.settings_dirty);
        assert_eq!(app.settings_draft, draft);
        assert_eq!(app.snapshot.roles, saved);

        // The next reload keeps the newer unsaved edits.
        let _ = app.update(Message::SnapshotLoaded(LabSnapshot {
            roles: RoleConfig::load(&path).expect("load roles"),
            ..LabSnapshot::default()
        }));
        assert!(app.settings_dirty);
        assert_eq!(app.settings_draft, draft);
    }

    #[test]
    fn revert_after_an_edit_during_a_save_returns_to_the_saved_config() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut app = temp_app(dir.path());
        let _ = app.update(Message::SettingsModelChanged(
            FactoryRole::Coder,
            "provider/model-1".to_string(),
        ));
        let saved = app.settings_draft.clone();

        let _ = app.update(Message::SettingsSavePressed);
        let _ = app.update(Message::SettingsModelChanged(
            FactoryRole::Coder,
            "provider/model-2".to_string(),
        ));
        let path = dir.path().join(ROLES_FILE);
        save_roles(&saved, &path).expect("save roles");
        let _ = app.update(Message::SettingsSaveFinished(Ok(()), saved.clone()));

        let _ = app.update(Message::SettingsRevertPressed);

        assert!(!app.settings_dirty);
        assert_eq!(app.settings_draft, saved);
        assert_eq!(app.settings_draft, app.snapshot.roles);
    }

    #[test]
    fn a_save_failure_keeps_the_draft_and_shows_the_banner() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut app = temp_app(dir.path());
        let _ = app.update(Message::SettingsModelChanged(
            FactoryRole::Coder,
            "provider/model-1".to_string(),
        ));
        let draft = app.settings_draft.clone();

        let _ = app.update(Message::SettingsSavePressed);
        let _ = app.update(Message::SettingsSaveFinished(
            Err("disk is read-only".to_string()),
            draft.clone(),
        ));

        assert!(!app.saving_settings);
        assert!(app.settings_dirty);
        assert_eq!(app.settings_draft, draft);
        assert!(app.errors_visible);
        assert!(
            app.settings_error
                .as_deref()
                .is_some_and(|error| error.contains("disk is read-only"))
        );
        assert!(
            app.banner_message()
                .is_some_and(|message| message.contains("disk is read-only"))
        );
    }

    #[test]
    fn an_invalid_config_is_refused_before_touching_disk() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut app = temp_app(dir.path());
        let mut invalid = RoleConfig::defaults();
        let mut settings = invalid
            .get(FactoryRole::Coder)
            .expect("coder settings")
            .clone();
        settings.model = String::new();
        invalid.set(FactoryRole::Coder, settings);
        app.settings_draft = invalid.clone();
        app.settings_dirty = true;
        let path = dir.path().join(ROLES_FILE);

        let task = app.update(Message::SettingsSavePressed);

        assert_eq!(task.units(), 0);
        assert!(!app.saving_settings);
        assert!(!path.exists());
        assert_eq!(app.settings_draft, invalid);
        assert!(app.settings_dirty);
        assert!(app.errors_visible);
        assert!(
            app.settings_error
                .as_deref()
                .is_some_and(|error| error.contains("invalid configuration"))
        );
    }
}

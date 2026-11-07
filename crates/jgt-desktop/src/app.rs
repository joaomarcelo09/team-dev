//! Application state and update logic (Elm style: `Msg` in, state change + `Task` out).

use crate::hub::{self, Feed};
use iced::{
    Subscription, Task, Theme,
    theme,
    widget::{markdown, operation, scrollable, text_editor},
    keyboard, window,
};
use jgt_core::{
    Decision, Direction, Event, Harness, Message, MessageKind, Question, ReconnectMode, Session, SessionStatus, Task as BoardTask,
    TaskEvent, TaskPriority, TaskStatus, TeamAgents,
    api::{MoveResponse, PatchSessionRequest},
    config::{Config, Desktop as DesktopConfig},
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const MESSAGES_ID: &str = "messages";
const PAGE: usize = 200;

#[derive(Debug, Clone, PartialEq)]
pub enum Conn {
    Connecting,
    Connected,
    Offline(String),
}

/// The messages of one session, with their parsed Markdown.
#[derive(Default)]
pub struct Thread {
    pub messages: Vec<Message>,
    pub markdown: HashMap<i64, Vec<markdown::Item>>,
    pub editors: HashMap<i64, text_editor::Content>,
    pub has_more: bool,
    pub loaded: bool,
}

impl Thread {
    pub(crate) fn upsert(&mut self, message: Message) {
        match self.messages.binary_search_by_key(&message.id, |m| m.id) {
            Ok(i) => {
                if self.messages[i].body != message.body
                    || !self.editors.contains_key(&message.id)
                    || !self.markdown.contains_key(&message.id)
                {
                    self.markdown.insert(message.id, crate::view::parse_markdown(&message.body));
                    self.editors.insert(message.id, text_editor::Content::with_text(&message.body));
                }
                self.messages[i] = message;
            }
            Err(i) => {
                self.markdown.insert(message.id, crate::view::parse_markdown(&message.body));
                self.editors.insert(message.id, text_editor::Content::with_text(&message.body));
                self.messages.insert(i, message);
            }
        }
    }

    fn set_question(&mut self, question: Question) {
        if let Some(m) = self.messages.iter_mut().find(|m| m.id == question.message_id) {
            m.question = Some(question);
        }
    }
}

pub struct Toast {
    pub text: String,
    pub error: bool,
    pub since: Instant,
}

/// Modules available for a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Module {
    Board,
    #[default]
    Coordinator,
    Worker(String),
}

/// What the settings form currently shows for one role.
// used by M5
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub role: String,
    pub harness: Harness,
    pub model: String,
    pub effort: String,
}

/// A team workspace: a project folder with its Coordinator.
// used from step 4 on
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub key: String,
    pub name: String,
    pub coordinator_id: Option<String>,
}

impl std::fmt::Display for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemePref {
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeStyle {
    #[default]
    Violet,
    Default,
    TokyoNight,
    Dracula,
}

impl ThemeStyle {
    pub fn parse(s: &str) -> Self {
        match s {
            "jg-violet" => Self::Violet,
            "default" => Self::Default,
            "tokyo-night" => Self::TokyoNight,
            "dracula" => Self::Dracula,
            _ => Self::Violet,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Violet => "jg-violet",
            Self::Default => "default",
            Self::TokyoNight => "tokyo-night",
            Self::Dracula => "dracula",
        }
    }
}

pub struct App {
    pub conn: Conn,
    pub sessions: Vec<Session>,
    pub selected: Option<String>,
    pub workspace: Option<String>,
    pub module: Module,
    pub threads: HashMap<String, Thread>,
    pub composer: text_editor::Content,
    /// Unsent text per session, kept while switching chats.
    pub drafts: HashMap<String, String>,
    /// Free-text answers being typed for questions.
    pub answers: HashMap<String, String>,
    pub search: String,
    pub show_archived: bool,
    /// Coordinators whose workers are collapsed in the sidebar.
    pub collapsed_coordinators: HashSet<String>,
    /// Whether the Worker section in the sidebar is collapsed.
    pub workers_collapsed: bool,
    pub rename: Option<String>,
    pub confirm_delete: bool,
    pub confirm_delete_team: bool,
    pub reconnecting: bool,
    pub toast: Option<Toast>,
    pub focused: bool,
    pub window: Option<window::Id>,
    pub at_bottom: bool,
    /// Session whose messages should snap to the end once loaded (e.g. freshly opened chat).
    pub newly_opened: Option<String>,
    pub theme_pref: ThemePref,
    pub theme_style: ThemeStyle,
    pub system_mode: theme::Mode,
    /// Whole-window scale (1.0 = 100%).
    pub zoom: f32,
    /// Chat text size in pixels.
    pub text_size: u16,
    pub popups: bool,
    pub show_settings: bool,
    /// Problems in the `[team]` config (unknown harness or effort, …), shown as a banner in Settings.
    pub config_warnings: Vec<String>,
    /// What the Settings agents form currently shows for each role.
    // used by M5
    #[allow(dead_code)]
    pub agents: Vec<AgentRow>,
    /// Harnesses whose program is installed here, for Settings.
    // used by M5
    #[allow(dead_code)]
    pub installed_harnesses: Vec<Harness>,
    /// Listed models or aliases per harness, populated asynchronously.
    // used by M5
    #[allow(dead_code)]
    pub harness_models: HashMap<Harness, Result<Vec<String>, String>>,
    /// Saved team agents if the selected workspace has a connected Coordinator, locking Settings.
    // used by M5
    #[allow(dead_code)]
    pub agents_locked: Option<TeamAgents>,
    /// Folder typed in the "New team" form, while it is open.
    pub new_team: Option<String>,
    pub starting_team: bool,
    /// The tasks of each Board opened so far, by workspace.
    pub boards: HashMap<String, crate::board::Board>,
    /// The task whose side panel is open.
    pub open_task: Option<String>,
    /// Activity logs loaded so far, by task id.
    pub task_events: HashMap<String, Vec<TaskEvent>>,
    /// The "+" form: its column and the title typed so far.
    pub new_task: Option<(TaskStatus, String)>,
    /// The open task's title and description as being edited.
    pub edit_title: String,
    pub edit_description: text_editor::Content,
    /// `edit_description`'s text, kept so drawing the panel need not rebuild it.
    pub edit_description_text: String,
    /// The open task's Details, parsed.
    pub task_details: Vec<markdown::Item>,
    pub confirm_delete_task: bool,
    /// Done shows all its tasks, not only the newest.
    pub show_all_done: bool,
    /// The comment being typed in the open task's panel (K13).
    pub comment: String,
    /// A card being dragged, and where it would land.
    pub drag: Option<crate::board::Drag>,
    /// A workspace whose queued task has no Coordinator to take it: the Board offers to start one.
    pub offer_team: Option<String>,
    /// The card or message currently in text-selection mode.
    pub selecting_message: Option<i64>,
    /// Highest message id seen, so only new messages mark the window urgent.
    last_message_id: i64,
    start_session: Option<String>,
}

/// A step for the zoom and text-size controls.
#[derive(Debug, Clone, Copy)]
pub enum Step {
    Up,
    Down,
    Reset,
}

#[derive(Debug, Clone)]
pub enum Msg {
    Feed(Feed),
    SessionsLoaded(Result<Vec<Session>, String>),
    Select(String),
    SelectWorkspace(String),
    SelectModule(Module),
    ThreadLoaded { session: String, older: bool, result: Result<Vec<Message>, String> },
    LoadOlder,
    Scrolled(scrollable::Viewport),
    Composer(text_editor::Action),
    Send,
    Sent(Result<Message, String>),
    AnswerDraft { question: String, text: String },
    Answer { question: String, answer: String },
    Answered(Result<Question, String>),
    SetListen(bool),
    SetMirror(bool),
    SetArchived(bool),
    Patched(Result<Session, String>),
    Reconnect(ReconnectMode),
    Reconnected(Result<String, String>),
    NewTeamOpen,
    NewTeamInput(String),
    NewTeamBrowse,
    NewTeamPicked(Option<String>),
    NewTeamStart,
    NewTeamCancel,
    TeamStarted(Result<String, String>),
    ToggleWorkers,
    BoardLoaded { workspace: String, result: Result<Vec<BoardTask>, String> },
    TaskEventsLoaded { task: String, result: Result<Vec<TaskEvent>, String> },
    OpenTask(String),
    CloseTask,
    NewTaskOpen(TaskStatus),
    NewTaskInput(String),
    NewTaskSave,
    NewTaskCancel,
    MoveTask { id: String, to: TaskStatus, position: Option<i64> },
    TaskMoved(Result<Box<MoveResponse>, String>),
    EditTitle(String),
    EditDescription(text_editor::Action),
    SaveTask,
    RevertTask,
    SetPriority(TaskPriority),
    /// A create, edit or decision came back from the hub.
    TaskChanged(Result<BoardTask, String>),
    DeleteTaskAsk,
    DeleteTaskCancel,
    DeleteTaskConfirm,
    TaskDeleted { id: String, result: Result<(), String> },
    Decide { id: String, choice: Decision },
    ShowAllDone(bool),
    CommentInput(String),
    CommentSend,
    /// The comment was saved; `Some(workspace)` when no Coordinator is there to read it.
    Commented(Result<Option<String>, String>),
    DragStart(String),
    DragOver(crate::board::Target),
    /// The mouse was released over a card or a column while dragging.
    Drop,
    /// The drag ended anywhere else: nothing moves.
    DragEnd,
    ExportBoard,
    Exported(Result<String, String>),
    StartTeamFor(String),
    DismissTeamOffer,
    RenameStart,
    RenameInput(String),
    RenameSave,
    RenameCancel,
    DeleteAsk,
    DeleteCancel,
    DeleteConfirm,
    Deleted(Result<(), String>),
    DeleteTeamAsk,
    DeleteTeamCancel,
    DeleteTeamConfirm,
    TeamDeleted(Result<(), String>),
    Search(String),
    ToggleArchived,
    ToggleCoordinatorCollapse(String),
    Copy(String),
    Link(markdown::Uri),
    Window(window::Id, window::Event),
    SystemTheme(theme::Mode),
    SetTheme(ThemePref),
    SetStyle(ThemeStyle),
    ToggleSettings,
    TogglePopups,
    // used by M5
    #[allow(dead_code)]
    SetAgentHarness(String, Harness),
    // used by M5
    #[allow(dead_code)]
    SetAgentModel(String, String),
    // used by M5
    #[allow(dead_code)]
    SetAgentEffort(String, String),
    ModelsListed(Harness, Result<Vec<String>, String>),
    // used by M5
    #[allow(dead_code)]
    SaveAgents,
    Zoom(Step),
    TextSize(Step),
    Key(keyboard::Event),
    ToggleSelect(i64),
    SelectMessage(i64),
    MessageEditor(i64, text_editor::Action),
    DismissToast,
    Tick,
    Done(Result<(), String>),
}

/// What to do with the chat scroll position when messages are loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDecision {
    SnapToEnd,
    KeepPosition,
}

impl ScrollDecision {
    pub fn should_snap(self) -> bool {
        self == Self::SnapToEnd
    }
}

/// Decides whether to snap to the latest messages or keep the current scroll position
/// when messages finish loading for a thread.
pub fn decide_thread_scroll(
    is_selected: bool,
    older: bool,
    newly_opened: bool,
    at_bottom: bool,
) -> ScrollDecision {
    if !is_selected || older {
        ScrollDecision::KeepPosition
    } else if newly_opened || at_bottom {
        ScrollDecision::SnapToEnd
    } else {
        ScrollDecision::KeepPosition
    }
}

impl App {
    pub fn new(start_session: Option<String>) -> (App, Task<Msg>) {
        let config = Config::load();
        let desktop = &config.desktop;
        let text_size = desktop.text_size.clamp(TEXT_SIZE_RANGE.0, TEXT_SIZE_RANGE.1);
        crate::style::set_text_size(text_size);
        let theme_pref = match desktop.theme.as_str() {
            "light" => ThemePref::Light,
            "dark" => ThemePref::Dark,
            _ => ThemePref::System,
        };
        let theme_style = ThemeStyle::parse(&desktop.style);
        let app = App {
            conn: Conn::Connecting,
            sessions: Vec::new(),
            selected: None,
            workspace: None,
            module: Module::Coordinator,
            threads: HashMap::new(),
            composer: text_editor::Content::new(),
            drafts: HashMap::new(),
            answers: HashMap::new(),
            search: String::new(),
            show_archived: false,
            collapsed_coordinators: HashSet::new(),
            workers_collapsed: false,
            rename: None,
            confirm_delete: false,
            confirm_delete_team: false,
            reconnecting: false,
            toast: None,
            focused: true,
            window: None,
            at_bottom: true,
            newly_opened: None,
            theme_pref,
            theme_style,
            system_mode: theme::Mode::Dark,
            zoom: clamp_zoom(desktop.zoom),
            text_size,
            popups: config.notifications.enabled,
            show_settings: false,
            config_warnings: config.team.warnings(None),
            agents: Vec::new(),
            installed_harnesses: Vec::new(),
            harness_models: HashMap::new(),
            agents_locked: None,
            new_team: None,
            starting_team: false,
            boards: HashMap::new(),
            open_task: None,
            task_events: HashMap::new(),
            new_task: None,
            edit_title: String::new(),
            edit_description: text_editor::Content::new(),
            edit_description_text: String::new(),
            task_details: Vec::new(),
            confirm_delete_task: false,
            show_all_done: false,
            comment: String::new(),
            drag: None,
            offer_team: None,
            selecting_message: None,
            last_message_id: 0,
            start_session,
        };
        let tasks = Task::batch([
            iced::system::theme().map(Msg::SystemTheme),
            window::latest().and_then(window::gain_focus),
        ]);
        (app, tasks)
    }

    pub fn title(&self) -> String {
        let unread: i64 = self.sessions.iter().filter(|s| !s.archived).map(|s| s.unread).sum();
        if unread > 0 { format!("({unread}) {}", jgt_core::APP_NAME) } else { jgt_core::APP_NAME.into() }
    }

    pub fn theme(&self) -> Theme {
        match self.theme_style {
            ThemeStyle::Violet => crate::style::violet(),
            ThemeStyle::TokyoNight => crate::style::tokyo_night(),
            ThemeStyle::Dracula => crate::style::dracula(),
            ThemeStyle::Default => {
                let dark = match self.theme_pref {
                    ThemePref::Dark => true,
                    ThemePref::Light => false,
                    ThemePref::System => self.system_mode != theme::Mode::Light,
                };
                if dark { crate::style::dark() } else { crate::style::light() }
            }
        }
    }

    pub fn subscription(&self) -> Subscription<Msg> {
        let mut subs = vec![
            Subscription::run(hub::feed).map(Msg::Feed),
            window::events().map(|(id, e)| Msg::Window(id, e)),
            iced::system::theme_changes().map(Msg::SystemTheme),
            keyboard::listen().map(Msg::Key),
        ];
        // The Team panel counts running workers' time.
        if self.toast.is_some() || self.sessions.iter().any(|s| s.is_worker() && s.status.is_connected()) {
            subs.push(iced::time::every(Duration::from_secs(1)).map(|_| Msg::Tick));
        }
        Subscription::batch(subs)
    }

    pub fn scale_factor(&self) -> f32 {
        self.zoom
    }

    /// Folders that had a Coordinator, newest first: quick picks for New team.
    pub fn recent_folders(&self) -> Vec<&str> {
        let mut coordinators: Vec<&Session> = self.sessions.iter().filter(|s| s.is_coordinator()).collect();
        coordinators.sort_by_key(|s| std::cmp::Reverse(s.last_message_at.unwrap_or(s.updated_at)));
        let mut folders: Vec<&str> = Vec::new();
        for cwd in coordinators.iter().filter_map(|s| s.cwd.as_deref()) {
            if !folders.contains(&cwd) && std::path::Path::new(cwd).is_dir() {
                folders.push(cwd);
            }
        }
        folders.truncate(5);
        folders
    }

    pub fn selected_session(&self) -> Option<&Session> {
        let id = self.selected.as_deref()?;
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn toggle_coordinator_collapse(&mut self, id: &str) {
        if !self.collapsed_coordinators.remove(id) {
            self.collapsed_coordinators.insert(id.to_string());
        }
    }

    pub fn is_coordinator_collapsed(&self, id: &str) -> bool {
        self.collapsed_coordinators.contains(id)
    }

    pub(crate) fn toast(&mut self, text: impl Into<String>, error: bool) {
        self.toast = Some(Toast { text: text.into(), error, since: Instant::now() });
    }

    fn sort_sessions(&mut self) {
        self.sessions.sort_by_key(|s| std::cmp::Reverse(s.last_message_at.unwrap_or(s.updated_at)));
    }

    fn upsert_session(&mut self, session: Session) {
        match self.sessions.iter_mut().find(|s| s.id == session.id) {
            Some(s) => *s = session,
            None => self.sessions.push(session),
        }
        self.sort_sessions();
    }

    fn load_thread(&self, session: String) -> Task<Msg> {
        let id = session.clone();
        Task::perform(hub::messages(id, None), move |result| Msg::ThreadLoaded {
            session: session.clone(),
            older: false,
            result,
        })
    }

    fn mark_read_if_visible(&mut self, session_id: &str) -> Task<Msg> {
        let visible = self.focused && self.selected.as_deref() == Some(session_id);
        let unread = self.sessions.iter().any(|s| s.id == session_id && s.unread > 0);
        if !(visible && unread) {
            return Task::none();
        }
        // The hub only reports the new counters; mark the visible messages read here too.
        let now = jgt_core::now_ms();
        if let Some(thread) = self.threads.get_mut(session_id) {
            for m in thread.messages.iter_mut().filter(|m| m.direction == Direction::FromAgent && m.read_at.is_none()) {
                m.read_at = Some(now);
            }
        }
        Task::perform(hub::mark_read(session_id.to_string()), Msg::Done)
    }

    fn presence(&self) -> Task<Msg> {
        Task::perform(hub::presence(self.focused, self.selected.clone()), Msg::Done)
    }

    fn snap(&mut self) -> Task<Msg> {
        self.at_bottom = true;
        operation::snap_to_end(MESSAGES_ID)
    }

    fn select(&mut self, id: String) -> Task<Msg> {
        if let Some(s) = self.sessions.iter().find(|s| s.id == id) {
            self.workspace = session_workspace(s, &self.sessions);
            self.module = if s.is_worker() {
                Module::Worker(id.clone())
            } else {
                Module::Coordinator
            };
        }
        self.close_task();
        self.new_task = None;
        if self.selected.as_deref() != Some(id.as_str()) {
            if let Some(prev) = self.selected.take() {
                self.drafts.insert(prev, self.composer.text());
            }
            let draft = self.drafts.remove(&id).unwrap_or_default();
            self.composer = text_editor::Content::with_text(draft.trim_end_matches('\n'));
            self.selected = Some(id.clone());
        } else if let Some(draft) = self.drafts.remove(&id) {
            self.composer = text_editor::Content::with_text(draft.trim_end_matches('\n'));
        }
        self.newly_opened = Some(id.clone());
        self.at_bottom = true;
        self.rename = None;
        self.confirm_delete = false;
        self.confirm_delete_team = false;
        self.selecting_message = None;
        Task::batch([
            self.snap(),
            self.load_selected_board(),
            self.load_thread(id.clone()),
            self.mark_read_if_visible(&id),
            self.presence(),
            operation::focus("composer"),
        ])
    }

    fn load_board(&self, workspace: String) -> Task<Msg> {
        let w = workspace.clone();
        Task::perform(hub::tasks(workspace), move |result| Msg::BoardLoaded { workspace: w.clone(), result })
    }

    /// Loads the workspace Board when the Board module is showing.
    fn load_selected_board(&self) -> Task<Msg> {
        if self.module == Module::Board
            && let Some(ws) = self.workspace.clone().or_else(|| self.selected_session().and_then(crate::board::workspace))
        {
            return self.load_board(ws);
        }
        Task::none()
    }

    fn load_task_events(&self, task: String) -> Task<Msg> {
        let id = task.clone();
        Task::perform(hub::task_events(task), move |result| Msg::TaskEventsLoaded { task: id.clone(), result })
    }

    fn find_task(&self, id: &str) -> Option<&BoardTask> {
        self.boards.values().find_map(|b| b.get(id))
    }

    /// The task whose side panel is open.
    pub fn open_task(&self) -> Option<&BoardTask> {
        self.find_task(self.open_task.as_deref()?)
    }

    fn close_task(&mut self) {
        if let Some(id) = self.open_task.take() {
            self.task_events.remove(&id);
        }
        self.confirm_delete_task = false;
        self.task_details.clear();
        self.comment.clear();
    }

    /// Takes a new version of a task: from the event stream or a reply to the app's own write.
    fn upsert_task(&mut self, task: BoardTask) {
        let Some(board) = self.boards.get_mut(&task.workspace) else { return };
        let old = board.get(&task.id).cloned();
        if !board.upsert(task.clone()) {
            return;
        }
        if self.open_task.as_deref() == Some(task.id.as_str()) {
            if task.archived {
                self.close_task();
            } else {
                self.refresh_open(old.as_ref(), &task);
            }
        }
    }

    fn remove_task(&mut self, id: &str) {
        for board in self.boards.values_mut() {
            board.remove(id);
        }
        if self.open_task.as_deref() == Some(id) {
            self.close_task();
        }
    }

    fn set_description(&mut self, text: &str) {
        self.edit_description = text_editor::Content::with_text(text);
        self.edit_description_text = text.to_string();
    }

    /// Updates the open panel to `task`. Text being edited stays unless it was unchanged from
    /// `old` (`None`: the panel just opened, take everything).
    fn refresh_open(&mut self, old: Option<&BoardTask>, task: &BoardTask) {
        if old.is_none_or(|o| self.edit_title == o.title) {
            self.edit_title = task.title.clone();
        }
        if old.is_none_or(|o| self.edit_description_text == o.description) {
            self.set_description(&task.description);
        }
        self.task_details = markdown::parse(task.details.as_deref().unwrap_or("")).collect();
    }

    fn patch(&self, req: PatchSessionRequest) -> Task<Msg> {
        match &self.selected {
            Some(id) => Task::perform(hub::patch(id.clone(), req), Msg::Patched),
            None => Task::none(),
        }
    }

    pub fn update(&mut self, msg: Msg) -> Task<Msg> {
        match msg {
            Msg::Feed(Feed::Connected) => {
                self.conn = Conn::Connected;
                let mut tasks = vec![Task::perform(hub::sessions(), Msg::SessionsLoaded), self.presence()];
                if let Some(id) = &self.selected {
                    tasks.push(self.load_thread(id.clone()));
                }
                // Board changes made while offline: reload what is on screen (K14).
                tasks.extend(self.boards.keys().map(|w| self.load_board(w.clone())));
                if let Some(id) = &self.open_task {
                    tasks.push(self.load_task_events(id.clone()));
                }
                Task::batch(tasks)
            }
            Msg::Feed(Feed::Disconnected(reason)) => {
                self.conn = Conn::Offline(reason);
                Task::none()
            }
            Msg::Feed(Feed::Event(event)) => self.on_event(*event),

            Msg::SessionsLoaded(Ok(sessions)) => {
                self.sessions = sessions;
                self.sort_sessions();
                if let Some(id) = self.selected.clone()
                    && !self.sessions.iter().any(|s| s.id == id) {
                        self.selected = None;
                    }
                match self.start_session.take() {
                    Some(id) if self.sessions.iter().any(|s| s.id == id) => self.select(id),
                    _ => {
                        if let Some(s) = self.selected.as_deref().and_then(|id| self.sessions.iter().find(|s| s.id == id)) {
                            self.workspace = session_workspace(s, &self.sessions);
                        }
                        Task::none()
                    }
                }
            }
            Msg::SessionsLoaded(Err(e)) => {
                self.toast(format!("Could not load sessions: {e}"), true);
                Task::none()
            }
            Msg::Select(id) => self.select(id),
            Msg::SelectWorkspace(ws) => {
                let ws_norm = jgt_core::team::normalize_folder(&ws).to_string();
                self.confirm_delete_team = false;
                self.workspace = Some(ws_norm.clone());
                self.module = Module::Coordinator;
                if let Some(coord) = coordinator_for(&self.sessions, &ws_norm) {
                    let id = coord.id.clone();
                    self.select(id)
                } else {
                    Task::none()
                }
            }
            Msg::SelectModule(module) => {
                let prev_board = self.module == Module::Board;
                self.module = module;
                if prev_board && self.module != Module::Board {
                    self.close_task();
                    self.new_task = None;
                }
                match &self.module {
                    Module::Board => {
                        if self.workspace.is_none()
                            && let Some(s) = self.selected_session()
                        {
                            self.workspace = session_workspace(s, &self.sessions);
                        }
                        if let Some(id) = &self.selected {
                            self.drafts.insert(id.clone(), self.composer.text());
                        }
                        if let Some(ws) = self.workspace.clone() {
                            self.load_board(ws)
                        } else {
                            Task::none()
                        }
                    }
                    Module::Coordinator => {
                        if let Some(ws) = &self.workspace
                            && let Some(coord) = coordinator_for(&self.sessions, ws)
                        {
                            let id = coord.id.clone();
                            return self.select(id);
                        }
                        Task::none()
                    }
                    Module::Worker(id) => {
                        let id = id.clone();
                        self.select(id)
                    }
                }
            }
            Msg::ThreadLoaded { session, older, result } => match result {
                Ok(messages) => {
                    let thread = self.threads.entry(session.clone()).or_default();
                    let full_page = messages.len() >= PAGE;
                    if !older {
                        *thread = Thread::default();
                    }
                    thread.loaded = true;
                    thread.has_more = full_page;
                    for m in messages {
                        thread.upsert(m);
                    }
                    let is_selected = self.selected.as_deref() == Some(session.as_str());
                    let is_newly_opened = self.newly_opened.as_deref() == Some(session.as_str());
                    if is_newly_opened {
                        self.newly_opened = None;
                    }
                    if decide_thread_scroll(is_selected, older, is_newly_opened, self.at_bottom).should_snap() {
                        return self.snap();
                    }
                    Task::none()
                }
                Err(e) => {
                    if self.newly_opened.as_deref() == Some(session.as_str()) {
                        self.newly_opened = None;
                    }
                    self.toast(format!("Could not load messages: {e}"), true);
                    Task::none()
                }
            },
            Msg::LoadOlder => {
                let Some(id) = self.selected.clone() else { return Task::none() };
                let before = self.threads.get(&id).and_then(|t| t.messages.first()).map(|m| m.id);
                let session = id.clone();
                Task::perform(hub::messages(id, before), move |result| Msg::ThreadLoaded {
                    session: session.clone(),
                    older: true,
                    result,
                })
            }
            Msg::Scrolled(viewport) => {
                self.at_bottom = viewport.relative_offset().y > 0.98;
                Task::none()
            }
            Msg::Composer(action) => {
                self.composer.perform(action);
                Task::none()
            }
            Msg::Send => {
                let text = self.composer.text().trim().to_string();
                let Some(session) = self.selected_session() else { return Task::none() };
                if text.is_empty() || session.status == SessionStatus::Ended {
                    return Task::none();
                }
                let session = session.clone();
                self.composer = text_editor::Content::new();
                Task::perform(hub::send_and_wake(session, text), Msg::Sent)
            }
            Msg::Sent(Ok(_)) => self.snap(),
            Msg::Sent(Err(e)) => {
                self.toast(format!("Not sent: {e}"), true);
                Task::none()
            }
            Msg::AnswerDraft { question, text } => {
                self.answers.insert(question, text);
                Task::none()
            }
            Msg::Answer { question, answer } => {
                let answer = answer.trim().to_string();
                if answer.is_empty() {
                    return Task::none();
                }
                self.answers.remove(&question);
                Task::perform(hub::answer(question, answer), Msg::Answered)
            }
            Msg::Answered(Ok(_)) => Task::none(),
            Msg::Answered(Err(e)) => {
                self.toast(format!("Answer not delivered: {e}"), true);
                Task::none()
            }
            Msg::SetListen(on) => self.patch(PatchSessionRequest { listen: Some(on), ..Default::default() }),
            Msg::SetMirror(on) => self.patch(PatchSessionRequest { mirror: Some(on), ..Default::default() }),
            Msg::SetArchived(on) => {
                self.toast(if on { "Archived" } else { "Restored from archive" }, false);
                self.patch(PatchSessionRequest { archived: Some(on), ..Default::default() })
            }
            Msg::Patched(Ok(session)) => {
                self.upsert_session(session);
                Task::none()
            }
            Msg::Patched(Err(e)) => {
                self.toast(e, true);
                Task::none()
            }
            Msg::Reconnect(mode) => {
                let Some(session) = self.selected_session().cloned() else { return Task::none() };
                self.reconnecting = true;
                Task::perform(hub::reconnect(session, mode), Msg::Reconnected)
            }
            Msg::NewTeamOpen => {
                let home = jgt_core::paths::home_dir().display().to_string();
                self.new_team = Some(format!("{home}/"));
                Task::none()
            }
            Msg::NewTeamInput(folder) => {
                self.new_team = Some(folder);
                Task::none()
            }
            Msg::NewTeamBrowse => {
                let start = self
                    .new_team
                    .as_deref()
                    .map(std::path::PathBuf::from)
                    .filter(|p| p.is_dir())
                    .unwrap_or_else(jgt_core::paths::home_dir);
                Task::perform(
                    async move {
                        rfd::AsyncFileDialog::new()
                            .set_title("Project folder for the new team")
                            .set_directory(start)
                            .pick_folder()
                            .await
                            .map(|f| f.path().display().to_string())
                    },
                    Msg::NewTeamPicked,
                )
            }
            Msg::NewTeamPicked(folder) => {
                if let Some(folder) = folder {
                    self.new_team = Some(folder);
                }
                Task::none()
            }
            Msg::NewTeamCancel => {
                self.new_team = None;
                Task::none()
            }
            Msg::NewTeamStart => {
                let Some(folder) = self.new_team.clone().filter(|f| !f.trim().is_empty()) else { return Task::none() };
                self.starting_team = true;
                Task::perform(hub::start_team(folder), Msg::TeamStarted)
            }
            Msg::TeamStarted(result) => {
                self.starting_team = false;
                match result {
                    Ok(line) => {
                        self.new_team = None;
                        self.offer_team = None;
                        self.toast(line, false);
                    }
                    Err(e) => self.toast(format!("Could not start the team: {e}"), true),
                }
                Task::none()
            }
            Msg::ToggleWorkers => {
                self.workers_collapsed = !self.workers_collapsed;
                Task::none()
            }
            Msg::BoardLoaded { workspace, result } => {
                match result {
                    Ok(tasks) => {
                        let old = self.open_task().cloned();
                        let board = crate::board::Board::reload(self.boards.get(&workspace), tasks);
                        self.boards.insert(workspace.clone(), board);
                        match (old, self.open_task().cloned()) {
                            (old, Some(task)) => self.refresh_open(old.as_ref(), &task),
                            // Deleted or archived while the app was offline.
                            (Some(old), None) if old.workspace == workspace => self.close_task(),
                            _ => {}
                        }
                    }
                    Err(e) => self.toast(format!("Could not load the Board: {e}"), true),
                }
                Task::none()
            }
            Msg::TaskEventsLoaded { task, result } => {
                match result {
                    Ok(events) => {
                        self.task_events.insert(task, events);
                    }
                    Err(e) => self.toast(format!("Could not load the task's log: {e}"), true),
                }
                Task::none()
            }
            Msg::OpenTask(id) => {
                let Some(task) = self.find_task(&id).cloned() else { return Task::none() };
                self.open_task = Some(id.clone());
                self.confirm_delete_task = false;
                self.refresh_open(None, &task);
                self.load_task_events(id)
            }
            Msg::CloseTask => {
                self.close_task();
                Task::none()
            }
            Msg::CommentInput(text) => {
                self.comment = text;
                Task::none()
            }
            Msg::CommentSend => {
                let Some(task) = self.open_task().cloned() else { return Task::none() };
                let text = self.comment.trim().to_string();
                if text.is_empty() {
                    return Task::none();
                }
                self.comment.clear();
                let workspace = task.workspace.clone();
                Task::perform(hub::comment(task.id, task.workspace, text), move |r| {
                    Msg::Commented(r.map(|heard| (!heard).then(|| workspace.clone())))
                })
            }
            Msg::Commented(result) => {
                match result {
                    Ok(None) => {}
                    // Saved in the log, but nobody reads it until a Coordinator runs.
                    Ok(Some(workspace)) => self.offer_team = Some(workspace),
                    Err(e) => self.toast(format!("Comment: {e}"), true),
                }
                Task::none()
            }
            Msg::DragStart(id) => {
                self.drag = Some(crate::board::Drag { id, over: None });
                Task::none()
            }
            Msg::DragOver(target) => {
                if let Some(drag) = &mut self.drag {
                    drag.over = Some(target);
                }
                Task::none()
            }
            Msg::Drop => {
                let Some(drag) = self.drag.take() else { return Task::none() };
                let Some(task) = self.find_task(&drag.id) else { return Task::none() };
                let to = self.boards.get(&task.workspace).zip(drag.over).and_then(|(b, t)| crate::board::drop_move(b, &drag.id, t));
                match to {
                    Some((to, position)) => self.update(Msg::MoveTask { id: drag.id, to, position }),
                    // Released where it was picked up: a click.
                    None => self.update(Msg::OpenTask(drag.id)),
                }
            }
            Msg::DragEnd => {
                self.drag = None;
                Task::none()
            }
            Msg::ExportBoard => {
                let Some(workspace) = self.workspace.clone().or_else(|| self.selected_session().and_then(crate::board::workspace)) else { return Task::none() };
                let Some(board) = self.boards.get(&workspace) else { return Task::none() };
                let text = jgt_core::board::to_markdown(board.tasks());
                let path = jgt_core::board::export_path(&workspace);
                Task::perform(
                    async move {
                        jgt_core::board::write_atomic(&path, &text).map_err(|e| e.to_string())?;
                        Ok(path.display().to_string())
                    },
                    Msg::Exported,
                )
            }
            Msg::Exported(result) => {
                match result {
                    Ok(path) => self.toast(format!("Board exported to {path}"), false),
                    Err(e) => self.toast(format!("Export failed: {e}"), true),
                }
                Task::none()
            }
            Msg::StartTeamFor(folder) => {
                self.starting_team = true;
                Task::perform(hub::start_team(folder), Msg::TeamStarted)
            }
            Msg::DismissTeamOffer => {
                self.offer_team = None;
                Task::none()
            }
            Msg::NewTaskOpen(column) => {
                self.new_task = Some((column, String::new()));
                operation::focus(crate::board::NEW_TASK_ID)
            }
            Msg::NewTaskInput(title) => {
                if let Some((_, t)) = &mut self.new_task {
                    *t = title;
                }
                Task::none()
            }
            Msg::NewTaskCancel => {
                self.new_task = None;
                Task::none()
            }
            Msg::NewTaskSave => {
                let Some(workspace) = self.workspace.clone().or_else(|| self.selected_session().and_then(crate::board::workspace)) else { return Task::none() };
                let Some((column, title)) = self.new_task.take() else { return Task::none() };
                let title = title.trim().to_string();
                if title.is_empty() {
                    return Task::none();
                }
                Task::perform(hub::create_task(workspace, title, column), Msg::TaskChanged)
            }
            Msg::MoveTask { id, to, position } => {
                Task::perform(hub::move_task(id, to, position), |r| Msg::TaskMoved(r.map(Box::new)))
            }
            Msg::TaskMoved(result) => {
                match result {
                    Ok(r) => {
                        if r.queued && r.coordinator.is_none() {
                            self.offer_team = Some(r.task.workspace.clone());
                        }
                        self.upsert_task(r.task);
                    }
                    Err(e) => self.toast(format!("Not moved: {e}"), true),
                }
                Task::none()
            }
            Msg::EditTitle(title) => {
                self.edit_title = title;
                Task::none()
            }
            Msg::EditDescription(action) => {
                let edit = action.is_edit();
                self.edit_description.perform(action);
                if edit {
                    self.edit_description_text = crate::board::editor_text(&self.edit_description);
                }
                Task::none()
            }
            Msg::SaveTask => {
                let Some(task) = self.open_task() else { return Task::none() };
                let title = self.edit_title.trim().to_string();
                let description = self.edit_description_text.clone();
                let title = (title != task.title && !title.is_empty()).then_some(title);
                let description = (description != task.description).then_some(description);
                if title.is_none() && description.is_none() {
                    return Task::none();
                }
                let id = task.id.clone();
                Task::perform(
                    hub::patch_task(id, move |req| {
                        req.title = title;
                        req.description = description;
                    }),
                    Msg::TaskChanged,
                )
            }
            Msg::RevertTask => {
                if let Some(task) = self.open_task().cloned() {
                    self.edit_title = task.title.clone();
                    self.set_description(&task.description);
                }
                Task::none()
            }
            Msg::SetPriority(priority) => {
                let Some(task) = self.open_task().filter(|t| t.priority != priority) else { return Task::none() };
                let id = task.id.clone();
                Task::perform(hub::patch_task(id, move |req| req.priority = Some(priority)), Msg::TaskChanged)
            }
            Msg::TaskChanged(result) => {
                match result {
                    Ok(task) => self.upsert_task(task),
                    Err(e) => self.toast(format!("Not saved: {e}"), true),
                }
                Task::none()
            }
            Msg::DeleteTaskAsk => {
                self.confirm_delete_task = true;
                Task::none()
            }
            Msg::DeleteTaskCancel => {
                self.confirm_delete_task = false;
                Task::none()
            }
            Msg::DeleteTaskConfirm => {
                self.confirm_delete_task = false;
                let Some(id) = self.open_task.clone() else { return Task::none() };
                let task = id.clone();
                Task::perform(hub::delete_task(id), move |result| Msg::TaskDeleted { id: task.clone(), result })
            }
            Msg::TaskDeleted { id, result } => {
                match result {
                    // No toast: the card going away says it, and a toast resets the Board's scrolling.
                    Ok(()) => self.remove_task(&id),
                    Err(e) => self.toast(format!("Not deleted: {e}"), true),
                }
                Task::none()
            }
            Msg::Decide { id, choice } => Task::perform(hub::decide(id, choice), Msg::TaskChanged),
            Msg::ShowAllDone(on) => {
                self.show_all_done = on;
                Task::none()
            }
            Msg::Reconnected(result) => {
                self.reconnecting = false;
                match result {
                    Ok(_) => self.toast("Starting the agent… it reconnects in a few seconds.", false),
                    Err(e) => self.toast(format!("Reconnect failed: {e}"), true),
                }
                Task::none()
            }
            Msg::RenameStart => {
                self.rename = self.selected_session().map(|s| s.display_name());
                operation::focus("rename")
            }
            Msg::RenameInput(text) => {
                self.rename = Some(text);
                Task::none()
            }
            Msg::RenameSave => {
                let Some(title) = self.rename.take() else { return Task::none() };
                self.patch(PatchSessionRequest { title: Some(title), ..Default::default() })
            }
            Msg::RenameCancel => {
                self.rename = None;
                Task::none()
            }
            Msg::DeleteAsk => {
                self.confirm_delete = true;
                Task::none()
            }
            Msg::DeleteCancel => {
                self.confirm_delete = false;
                Task::none()
            }
            Msg::DeleteConfirm => {
                self.confirm_delete = false;
                match self.selected.clone() {
                    Some(id) => Task::perform(hub::delete(id), Msg::Deleted),
                    None => Task::none(),
                }
            }
            Msg::Deleted(Ok(())) => {
                self.toast("Conversation deleted", false);
                Task::none()
            }
            Msg::Deleted(Err(e)) | Msg::Done(Err(e)) => {
                self.toast(e, true);
                Task::none()
            }
            Msg::DeleteTeamAsk => {
                self.confirm_delete_team = true;
                Task::none()
            }
            Msg::DeleteTeamCancel => {
                self.confirm_delete_team = false;
                Task::none()
            }
            Msg::DeleteTeamConfirm => {
                self.confirm_delete_team = false;
                match self.workspace.clone() {
                    Some(ws) => Task::perform(hub::delete_workspace(ws), Msg::TeamDeleted),
                    None => Task::none(),
                }
            }
            Msg::TeamDeleted(Ok(())) => {
                self.toast("Team deleted", false);
                if let Some(ws) = self.workspace.take() {
                    self.boards.remove(&ws);
                    self.boards.remove(&format!("{ws}/"));
                    if let Some(s) = self.selected_session()
                        && session_workspace(s, &self.sessions).as_deref() == Some(&ws)
                    {
                        self.selected = None;
                    }
                    if let Some(id) = &self.selected
                        && !self.sessions.iter().any(|s| s.id == *id)
                    {
                        self.selected = None;
                    }
                    let other = workspaces(&self.sessions).into_iter().find(|w| w.key != ws);
                    let select_task = if let Some(other) = other {
                        self.workspace = Some(other.key.clone());
                        self.module = Module::Coordinator;
                        if let Some(coord) = coordinator_for(&self.sessions, &other.key) {
                            self.select(coord.id.clone())
                        } else {
                            Task::none()
                        }
                    } else {
                        self.workspace = None;
                        Task::none()
                    };
                    Task::batch([select_task, Task::perform(hub::sessions(), Msg::SessionsLoaded)])
                } else {
                    Task::perform(hub::sessions(), Msg::SessionsLoaded)
                }
            }
            Msg::TeamDeleted(Err(e)) => {
                self.toast(e, true);
                Task::none()
            }
            Msg::Done(Ok(())) => Task::none(),
            Msg::Search(text) => {
                self.search = text;
                Task::none()
            }
            Msg::ToggleArchived => {
                self.show_archived = !self.show_archived;
                Task::none()
            }
            Msg::ToggleCoordinatorCollapse(id) => {
                self.toggle_coordinator_collapse(&id);
                Task::none()
            }
            Msg::Copy(text) => {
                self.toast("Copied", false);
                iced::clipboard::write(text)
            }
            Msg::ToggleSelect(id) => {
                self.selecting_message = if self.selecting_message == Some(id) { None } else { Some(id) };
                Task::none()
            }
            Msg::SelectMessage(id) => {
                self.selecting_message = Some(id);
                Task::none()
            }
            Msg::MessageEditor(id, action) => {
                if !action.is_edit()
                    && let Some(thread) = self.selected.as_ref().and_then(|s| self.threads.get_mut(s))
                    && let Some(editor) = thread.editors.get_mut(&id)
                {
                    editor.perform(action);
                }
                Task::none()
            }
            Msg::Link(url) => {
                let _ = std::process::Command::new("xdg-open")
                    .arg(&url)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .map(|mut c| std::thread::spawn(move || c.wait()));
                Task::none()
            }
            Msg::Window(id, event) => {
                self.window = Some(id);
                match event {
                    window::Event::Focused => {
                        self.focused = true;
                        let read = self.selected.clone().map(|s| self.mark_read_if_visible(&s)).unwrap_or_else(Task::none);
                        // Clear the urgent mark set by `attention` (most WMs also do this on focus).
                        Task::batch([self.presence(), read, window::request_user_attention(id, None)])
                    }
                    window::Event::Unfocused => {
                        self.focused = false;
                        self.presence()
                    }
                    _ => Task::none(),
                }
            }
            Msg::SystemTheme(mode) => {
                self.system_mode = mode;
                Task::none()
            }
            Msg::SetTheme(pref) => {
                self.theme_pref = pref;
                self.save_view_settings();
                Task::none()
            }
            Msg::SetStyle(style) => {
                self.theme_style = style;
                self.save_view_settings();
                Task::none()
            }
            Msg::ToggleSettings => {
                self.show_settings = !self.show_settings;
                if self.show_settings {
                    let config = Config::load();
                    self.popups = config.notifications.enabled;
                    self.config_warnings = config.team.warnings(None);
                    self.agents_locked = agents_locked_for(self);

                    self.agents = if self.agents_locked.is_some() {
                        let folder: PathBuf = self
                            .workspace
                            .as_ref()
                            .map(PathBuf::from)
                            .unwrap_or_else(jgt_core::paths::home_dir);
                        jgt_core::config::roles()
                            .map(|role| {
                                let settings = config.team.for_skill(role, &folder);
                                AgentRow {
                                    role: role.to_string(),
                                    harness: settings.harness,
                                    model: settings.model,
                                    effort: settings.effort,
                                }
                            })
                            .collect()
                    } else {
                        jgt_core::config::roles()
                            .map(|role| {
                                let settings = config.team.for_user(role);
                                AgentRow {
                                    role: role.to_string(),
                                    harness: settings.harness,
                                    model: settings.model,
                                    effort: settings.effort,
                                }
                            })
                            .collect()
                    };

                    self.installed_harnesses = jgt_core::harness::installed(&config);
                    self.harness_models.clear();

                    let tasks = self
                        .installed_harnesses
                        .iter()
                        .copied()
                        .map(|h| {
                            let cfg = config.clone();
                            Task::perform(
                                async move {
                                    match tokio::task::spawn_blocking(move || {
                                        jgt_core::harness::models(h, &cfg)
                                    })
                                    .await
                                    {
                                        Ok(res) => res,
                                        Err(e) => Err(format!("failed to list models: {e}")),
                                    }
                                },
                                move |res| Msg::ModelsListed(h, res),
                            )
                        })
                        .collect::<Vec<_>>();

                    Task::batch(tasks)
                } else {
                    Task::none()
                }
            }
            Msg::TogglePopups => {
                self.popups = !self.popups;
                let mut config = Config::load();
                apply_popups(&mut config, self.popups);
                let _ = config.save();
                Task::perform(hub::reload_config(), Msg::Done)
            }
            Msg::SetAgentHarness(role, harness) => {
                if role == jgt_core::model::COORDINATOR {
                    return Task::none();
                }
                if let Some(agent) = self.agents.iter_mut().find(|a| a.role == role) {
                    agent.harness = harness;
                    let first_model = self
                        .harness_models
                        .get(&harness)
                        .and_then(|r| r.as_ref().ok())
                        .and_then(|models| models.first())
                        .cloned()
                        .unwrap_or_default();
                    agent.model = first_model;
                    let levels = jgt_core::harness::effort_levels(harness);
                    if !levels.is_empty() && !levels.contains(&agent.effort.as_str()) {
                        let effort = jgt_core::harness::nearest_effort(harness, &agent.effort).unwrap_or(levels[0]);
                        agent.effort = effort.to_string();
                    }
                }
                Task::none()
            }
            Msg::SetAgentModel(role, model) => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.role == role) {
                    agent.model = model;
                }
                Task::none()
            }
            Msg::SetAgentEffort(role, effort) => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.role == role) {
                    agent.effort = effort;
                }
                Task::none()
            }
            Msg::ModelsListed(harness, result) => {
                self.harness_models.insert(harness, result);
                Task::none()
            }
            Msg::SaveAgents => {
                let mut config = Config::load();
                apply_agents(&mut config, &self.agents);
                let _ = config.save();
                self.config_warnings = config.team.warnings(None);
                self.toast("Applies to teams started from now on", false);
                Task::perform(hub::reload_config(), Msg::Done)
            }
            Msg::Zoom(step) => {
                // 10% steps, rounded so repeated steps don't drift.
                let percent = (self.zoom * 100.0).round() as i32;
                self.zoom = clamp_zoom(match step {
                    Step::Up => (percent + 10) as f32 / 100.0,
                    Step::Down => (percent - 10) as f32 / 100.0,
                    Step::Reset => 1.0,
                });
                self.save_view_settings();
                Task::none()
            }
            Msg::TextSize(step) => {
                self.text_size = match step {
                    Step::Up => self.text_size + 1,
                    Step::Down => self.text_size.saturating_sub(1),
                    Step::Reset => DesktopConfig::DEFAULT_TEXT_SIZE,
                }
                .clamp(TEXT_SIZE_RANGE.0, TEXT_SIZE_RANGE.1);
                crate::style::set_text_size(self.text_size);
                self.save_view_settings();
                Task::none()
            }
            Msg::Key(event) => match event {
                keyboard::Event::KeyPressed { key, modifiers, .. } => match zoom_shortcut(&key, modifiers) {
                    Some(step) => self.update(Msg::Zoom(step)),
                    None if self.show_settings && key == keyboard::Key::Named(keyboard::key::Named::Escape) => {
                        self.show_settings = false;
                        Task::none()
                    }
                    None if self.drag.is_some() && key == keyboard::Key::Named(keyboard::key::Named::Escape) => {
                        self.drag = None;
                        Task::none()
                    }
                    None if self.open_task.is_some() && key == keyboard::Key::Named(keyboard::key::Named::Escape) => {
                        self.close_task();
                        Task::none()
                    }
                    None if self.selecting_message.is_some() && key == keyboard::Key::Named(keyboard::key::Named::Escape) => {
                        self.selecting_message = None;
                        Task::none()
                    }
                    None => Task::none(),
                },
                _ => Task::none(),
            },
            Msg::DismissToast => {
                self.toast = None;
                Task::none()
            }
            Msg::Tick => {
                if self.toast.as_ref().is_some_and(|t| t.since.elapsed() > Duration::from_secs(if t.error { 8 } else { 3 })) {
                    self.toast = None;
                }
                Task::none()
            }
        }
    }

    fn on_event(&mut self, event: Event) -> Task<Msg> {
        match event {
            Event::Hello { .. } => Task::none(),
            Event::Task { task } => {
                self.upsert_task(task);
                Task::none()
            }
            Event::TaskRemoved { id, .. } => {
                self.remove_task(&id);
                Task::none()
            }
            Event::TaskEvent { event } => {
                if let Some(log) = self.task_events.get_mut(&event.task_id)
                    && !log.iter().any(|e| e.id == event.id)
                {
                    log.push(event);
                }
                Task::none()
            }
            Event::Session { session } => {
                let id = session.id.clone();
                self.upsert_session(session);
                self.mark_read_if_visible(&id)
            }
            Event::SessionRemoved { id } => {
                self.sessions.retain(|s| s.id != id);
                self.threads.remove(&id);
                if self.selected.as_deref() == Some(id.as_str()) {
                    self.selected = None;
                }
                Task::none()
            }
            Event::Message { message } => {
                let attention = self.attention(&message);
                let session = message.session_id.clone();
                let is_selected = self.selected.as_deref() == Some(session.as_str());
                if let Some(thread) = self.threads.get_mut(&session).filter(|t| t.loaded) {
                    let is_new = thread.messages.binary_search_by_key(&message.id, |m| m.id).is_err();
                    thread.upsert(message);
                    if is_selected && is_new && self.at_bottom {
                        return Task::batch([self.snap(), attention]);
                    }
                }
                attention
            }
            Event::Question { question } => {
                if let Some(thread) = self.threads.get_mut(&question.session_id) {
                    thread.set_question(question);
                }
                Task::none()
            }
            Event::Focus { session_id } => {
                let select = match session_id {
                    Some(id) if self.sessions.iter().any(|s| s.id == id) => self.select(id),
                    Some(id) => {
                        self.start_session = Some(id);
                        Task::perform(hub::sessions(), Msg::SessionsLoaded)
                    }
                    None => Task::none(),
                };
                let focus = match self.window {
                    Some(id) => window::gain_focus(id),
                    None => window::latest().and_then(window::gain_focus),
                };
                Task::batch([select, focus])
            }
        }
    }
}

const TEXT_SIZE_RANGE: (u16, u16) = DesktopConfig::TEXT_SIZE_RANGE;

fn clamp_zoom(zoom: f32) -> f32 {
    let (min, max) = DesktopConfig::ZOOM_RANGE;
    if zoom.is_finite() { zoom.clamp(min, max) } else { 1.0 }
}

/// Ctrl + / Ctrl - / Ctrl 0, as in a browser.
pub fn zoom_shortcut(key: &keyboard::Key, modifiers: keyboard::Modifiers) -> Option<Step> {
    if !modifiers.command() {
        return None;
    }
    match key.as_ref() {
        keyboard::Key::Character("+" | "=") => Some(Step::Up),
        keyboard::Key::Character("-" | "_") => Some(Step::Down),
        keyboard::Key::Character("0") => Some(Step::Reset),
        _ => None,
    }
}

impl App {
    /// Marks the window urgent for a new agent message while it is not focused, like Teams or
    /// Discord: i3 (X11 urgency hint) and Hyprland (xdg-activation) highlight the window and its
    /// workspace until it gets focus.
    fn attention(&mut self, message: &Message) -> Task<Msg> {
        let new = message.id > self.last_message_id;
        self.last_message_id = self.last_message_id.max(message.id);
        let from_agent = message.direction == Direction::FromAgent;
        // Events re-sent for old messages (delivery or read updates) must not ring.
        let recent = jgt_core::now_ms() - message.created_at < 60_000;
        match self.window {
            Some(id) if new && from_agent && recent && !self.focused => {
                let level = match message.kind {
                    MessageKind::Question | MessageKind::Permission => window::UserAttention::Critical,
                    _ => window::UserAttention::Informational,
                };
                window::request_user_attention(id, Some(level))
            }
            _ => Task::none(),
        }
    }

    /// Saves theme, style, zoom and text size to config.toml (other settings there are kept).
    fn save_view_settings(&self) {
        let mut config = Config::load();
        config.desktop.theme = match self.theme_pref {
            ThemePref::System => "system",
            ThemePref::Dark => "dark",
            ThemePref::Light => "light",
        }
        .into();
        config.desktop.style = self.theme_style.as_str().into();
        config.desktop.zoom = self.zoom;
        config.desktop.text_size = self.text_size;
        let _ = config.save();
    }
}

/// Applies the desktop pop-ups setting to a config.
pub(crate) fn apply_popups(config: &mut Config, enabled: bool) {
    config.notifications.enabled = enabled;
}

/// Applies agent row settings to `config.team.skills`, keeping other skill settings intact.
pub(crate) fn apply_agents(config: &mut Config, agents: &[AgentRow]) {
    for agent in agents {
        let skill = config.team.skills.entry(agent.role.clone()).or_default();
        skill.harness = Some(agent.harness.as_str().to_string());
        skill.model = Some(agent.model.clone());
        skill.effort = Some(agent.effort.clone());
    }
}

pub use jgt_core::team::{coordinator_for, session_workspace};

/// Saved agents if the selected workspace has a connected Coordinator session (user decision 1a).
// used from M4 on
#[allow(dead_code)]
pub fn agents_locked_for(app: &App) -> Option<TeamAgents> {
    let ws = app.workspace.as_deref()?;
    let coord = coordinator_for(&app.sessions, ws)?;
    if coord.status.is_connected() {
        TeamAgents::load(Path::new(ws))
    } else {
        None
    }
}

/// The workspaces represented in `sessions`: unique folders that have a Coordinator,
/// sorted stably by name.
// used from step 4 on
#[allow(dead_code)]
pub fn workspaces(sessions: &[Session]) -> Vec<Workspace> {
    let mut keys: Vec<String> = sessions
        .iter()
        .filter(|s| s.is_coordinator())
        .filter_map(|s| {
            s.cwd
                .as_deref()
                .map(jgt_core::team::normalize_folder)
                .filter(|k| !k.is_empty())
                .map(str::to_string)
        })
        .collect();
    keys.sort();
    keys.dedup();

    let mut list = Vec::new();
    for key in keys {
        let coord = coordinator_for(sessions, &key);
        let name = coord.and_then(|c| c.project()).unwrap_or_else(|| {
            std::path::Path::new(&key)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| key.clone())
        });
        list.push(Workspace {
            name,
            coordinator_id: coord.map(|c| c.id.clone()),
            key,
        });
    }

    list.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.key.cmp(&b.key)));
    list
}

/// Partitions team workers into active (working) and idle/done workers.
// used from step 4 on
#[allow(dead_code)]
pub fn partition_team_workers<'a>(
    workers: &[&'a Session],
    show_archived: bool,
) -> (Vec<&'a Session>, Vec<&'a Session>) {
    let mut active = Vec::new();
    let mut idle = Vec::new();
    for &w in workers {
        if !w.archived || show_archived {
            if w.status == SessionStatus::Active {
                active.push(w);
            } else {
                idle.push(w);
            }
        }
    }
    (active, idle)
}

/// The workers for `ws`: non-coordinator, non-archived sessions of that folder,
/// in the same ordering the team panel uses.
// used from step 4 on
#[allow(dead_code)]
pub fn workers_for<'a>(sessions: &'a [Session], ws: &str) -> Vec<&'a Session> {
    let Some(coord) = coordinator_for(sessions, ws) else {
        return Vec::new();
    };
    let workers: Vec<&Session> = sessions
        .iter()
        .filter(|s| !s.is_coordinator() && s.parent_id.as_deref() == Some(coord.id.as_str()))
        .collect();
    let (mut active, idle) = partition_team_workers(&workers, false);
    active.extend(idle);
    active
}

#[cfg(test)]
mod tests {
    use super::*;
    use jgt_core::{Delivery, Direction, MessageKind};

    fn test_message(id: i64, body: &str, read_at: Option<i64>) -> Message {
        Message {
            id,
            session_id: "s1".to_string(),
            direction: Direction::ToAgent,
            kind: MessageKind::Text,
            body: body.to_string(),
            meta: serde_json::Value::Null,
            delivery: Delivery::Delivered,
            created_at: 1000,
            delivered_at: Some(1000),
            read_at,
            question: None,
        }
    }

    #[test]
    fn thread_upsert_rebuilds_editor_only_when_body_changes() {
        let mut thread = Thread::default();
        let msg = test_message(1, "hello world", None);
        thread.upsert(msg);

        // Make a selection in the editor.
        thread.editors.get_mut(&1).unwrap().perform(text_editor::Action::SelectLine);
        assert_eq!(thread.editors.get(&1).unwrap().selection().as_deref(), Some("hello world"));

        // Upsert with updated status/read_at, same body.
        let updated_status = test_message(1, "hello world", Some(2000));
        thread.upsert(updated_status);
        assert_eq!(thread.messages[0].read_at, Some(2000));
        // Selection should be preserved!
        assert_eq!(
            thread.editors.get(&1).unwrap().selection().as_deref(),
            Some("hello world"),
            "Editor must not be recreated on status update when body is unchanged"
        );

        // Upsert with new body.
        let updated_body = test_message(1, "goodbye world", Some(3000));
        thread.upsert(updated_body);
        assert_eq!(thread.messages[0].body, "goodbye world");
        // Editor was rebuilt, so old selection is gone and new text is present.
        assert_eq!(thread.editors.get(&1).unwrap().text().trim(), "goodbye world");
        assert_eq!(thread.editors.get(&1).unwrap().selection(), None);
    }

    #[test]
    fn message_selection_state_transitions() {
        let (mut app, _) = App::new(None);
        assert_eq!(app.selecting_message, None);

        // Clicking a message enters selection mode for that message
        let _ = app.update(Msg::SelectMessage(1));
        assert_eq!(app.selecting_message, Some(1));

        // Re-clicking the same message keeps selection mode active
        let _ = app.update(Msg::SelectMessage(1));
        assert_eq!(app.selecting_message, Some(1));

        // Selecting another message transitions directly to that message
        let _ = app.update(Msg::SelectMessage(2));
        assert_eq!(app.selecting_message, Some(2));

        // Toggle button on the currently selected message exits selection mode
        let _ = app.update(Msg::ToggleSelect(2));
        assert_eq!(app.selecting_message, None);

        // Toggle button on unselected message enters selection mode
        let _ = app.update(Msg::ToggleSelect(3));
        assert_eq!(app.selecting_message, Some(3));

        // Escape exits selection mode
        let _ = app.update(Msg::Key(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        }));
        assert_eq!(app.selecting_message, None);
    }

    #[test]
    fn applying_popups_toggle_updates_config() {
        let mut config = Config::default();
        assert!(config.notifications.enabled);
        apply_popups(&mut config, false);
        assert!(!config.notifications.enabled);
        apply_popups(&mut config, true);
        assert!(config.notifications.enabled);
    }

    #[test]
    fn theme_style_parse_and_fallback() {
        assert_eq!(ThemeStyle::parse("jg-violet"), ThemeStyle::Violet);
        assert_eq!(ThemeStyle::parse("default"), ThemeStyle::Default);
        assert_eq!(ThemeStyle::parse("tokyo-night"), ThemeStyle::TokyoNight);
        assert_eq!(ThemeStyle::parse("dracula"), ThemeStyle::Dracula);

        // Unknown values and empty string fall back to Violet
        assert_eq!(ThemeStyle::parse("unknown"), ThemeStyle::Violet);
        assert_eq!(ThemeStyle::parse("nord"), ThemeStyle::Violet);
        assert_eq!(ThemeStyle::parse(""), ThemeStyle::Violet);

        // Round-trip through as_str
        for style in [ThemeStyle::Violet, ThemeStyle::Default, ThemeStyle::TokyoNight, ThemeStyle::Dracula] {
            assert_eq!(ThemeStyle::parse(style.as_str()), style);
        }
    }

    #[test]
    fn app_theme_violet_ignores_theme_pref() {
        let (mut app, _) = App::new(None);
        app.theme_style = ThemeStyle::Violet;
        for pref in [ThemePref::Dark, ThemePref::Light, ThemePref::System] {
            app.theme_pref = pref;
            let theme = app.theme();
            assert_eq!(theme.palette().background, iced::Color::from_rgb8(0x11, 0x13, 0x19));
            assert_eq!(theme.palette().primary, iced::Color::from_rgb8(0x7c, 0x3a, 0xed));
        }
    }

    #[test]
    fn coordinator_collapse_toggles_and_preserves_state() {
        let (mut app, _) = App::new(None);
        assert!(!app.is_coordinator_collapsed("coord-1"));
        assert!(!app.is_coordinator_collapsed("coord-2"));

        // Toggle coord-1 to collapsed.
        let _ = app.update(Msg::ToggleCoordinatorCollapse("coord-1".to_string()));
        assert!(app.is_coordinator_collapsed("coord-1"));
        assert!(!app.is_coordinator_collapsed("coord-2"));

        // Toggle coord-2 to collapsed.
        let _ = app.update(Msg::ToggleCoordinatorCollapse("coord-2".to_string()));
        assert!(app.is_coordinator_collapsed("coord-1"));
        assert!(app.is_coordinator_collapsed("coord-2"));

        // Toggle coord-1 back to expanded.
        let _ = app.update(Msg::ToggleCoordinatorCollapse("coord-1".to_string()));
        assert!(!app.is_coordinator_collapsed("coord-1"));
        assert!(app.is_coordinator_collapsed("coord-2"));
    }

    #[test]
    fn collapsed_state_survives_sessions_refresh() {
        let (mut app, _) = App::new(None);
        let _ = app.update(Msg::ToggleCoordinatorCollapse("coord-1".to_string()));
        assert!(app.is_coordinator_collapsed("coord-1"));

        // Simulate session list refresh from hub.
        let session = serde_json::from_value::<Session>(serde_json::json!({
            "id": "coord-1", "harness": jgt_core::Harness::ClaudeCode, "external_id": "ext-1",
            "cwd": "/w/", "title": null, "status": SessionStatus::Active,
            "listen": false, "mirror": false, "archived": false, "unread": 0, "queued": 0,
            "created_at": 0, "updated_at": 0, "last_seen_at": 0, "awaiting_reply": false,
            "role": jgt_core::COORDINATOR,
        }))
        .expect("session");

        let _ = app.update(Msg::SessionsLoaded(Ok(vec![session])));
        assert!(app.is_coordinator_collapsed("coord-1"), "Collapsed coordinators must survive session list reload");
    }

    #[test]
    fn leaving_board_closes_open_task_and_new_task() {
        let (mut app, _) = App::new(None);
        app.module = Module::Board;
        app.open_task = Some("task-1".to_string());
        app.new_task = Some((jgt_core::TaskStatus::Backlog, "New task".to_string()));

        // Leaving board via SelectModule
        let _ = app.update(Msg::SelectModule(Module::Coordinator));
        assert!(app.open_task.is_none());
        assert!(app.new_task.is_none());

        // Re-open and test via select()
        app.open_task = Some("task-2".to_string());
        app.new_task = Some((jgt_core::TaskStatus::Backlog, "New task 2".to_string()));
        let _ = app.select("some-session".to_string());
        assert!(app.open_task.is_none());
        assert!(app.new_task.is_none());
    }

    fn make_session(
        id: &str,
        role: Option<&str>,
        parent_id: Option<&str>,
        cwd: Option<&str>,
        status: SessionStatus,
        archived: bool,
    ) -> Session {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "harness": jgt_core::Harness::ClaudeCode,
            "external_id": format!("ext-{id}"),
            "cwd": cwd.unwrap_or("/w/"),
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
    fn helpers_empty_sessions() {
        let sessions: Vec<Session> = vec![];
        assert_eq!(workspaces(&sessions), vec![]);
        assert_eq!(coordinator_for(&sessions, "/w"), None);
        assert!(workers_for(&sessions, "/w").is_empty());
    }

    #[test]
    fn helpers_one_team() {
        let c1 = make_session("c1", Some(jgt_core::COORDINATOR), None, Some("/repo/team-a"), SessionStatus::Active, false);
        let w1 = make_session("w1", Some("coder"), Some("c1"), Some("/repo/team-a"), SessionStatus::Active, false);
        let sessions = vec![c1.clone(), w1.clone()];

        let ws = workspaces(&sessions);
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].key, "/repo/team-a");
        assert_eq!(ws[0].name, "team-a");
        assert_eq!(ws[0].coordinator_id.as_deref(), Some("c1"));

        assert_eq!(coordinator_for(&sessions, "/repo/team-a").map(|s| s.id.as_str()), Some("c1"));
        assert_eq!(coordinator_for(&sessions, "/repo/team-a/").map(|s| s.id.as_str()), Some("c1"));

        let workers = workers_for(&sessions, "/repo/team-a");
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0].id, "w1");
    }

    #[test]
    fn helpers_two_teams_sorted_stably_by_name() {
        let c_zeta = make_session("c_zeta", Some(jgt_core::COORDINATOR), None, Some("/projects/zeta"), SessionStatus::Active, false);
        let w_zeta = make_session("w_zeta", Some("coder"), Some("c_zeta"), Some("/projects/zeta"), SessionStatus::Active, false);
        let c_alpha = make_session("c_alpha", Some(jgt_core::COORDINATOR), None, Some("/projects/alpha"), SessionStatus::Active, false);
        let w_alpha = make_session("w_alpha", Some("reviewer"), Some("c_alpha"), Some("/projects/alpha"), SessionStatus::Active, false);
        let sessions = vec![c_zeta, w_zeta, c_alpha, w_alpha];

        let ws = workspaces(&sessions);
        assert_eq!(ws.len(), 2);
        assert_eq!(ws[0].name, "alpha");
        assert_eq!(ws[0].key, "/projects/alpha");
        assert_eq!(ws[0].coordinator_id.as_deref(), Some("c_alpha"));
        assert_eq!(ws[1].name, "zeta");
        assert_eq!(ws[1].key, "/projects/zeta");
        assert_eq!(ws[1].coordinator_id.as_deref(), Some("c_zeta"));

        assert_eq!(coordinator_for(&sessions, "/projects/alpha").map(|s| s.id.as_str()), Some("c_alpha"));
        assert_eq!(coordinator_for(&sessions, "/projects/zeta").map(|s| s.id.as_str()), Some("c_zeta"));

        let alpha_workers = workers_for(&sessions, "/projects/alpha");
        assert_eq!(alpha_workers.len(), 1);
        assert_eq!(alpha_workers[0].id, "w_alpha");

        let zeta_workers = workers_for(&sessions, "/projects/zeta");
        assert_eq!(zeta_workers.len(), 1);
        assert_eq!(zeta_workers[0].id, "w_zeta");
    }

    #[test]
    fn helpers_archived_workers_excluded() {
        let c1 = make_session("c1", Some(jgt_core::COORDINATOR), None, Some("/team"), SessionStatus::Active, false);
        let w_active = make_session("w1", Some("coder"), Some("c1"), Some("/team"), SessionStatus::Active, false);
        let w_archived = make_session("w2", Some("tester"), Some("c1"), Some("/team"), SessionStatus::Ended, true);
        let sessions = vec![c1, w_active, w_archived];

        let workers = workers_for(&sessions, "/team");
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0].id, "w1");
    }

    #[test]
    fn helpers_session_without_a_team() {
        let c1 = make_session("c1", Some(jgt_core::COORDINATOR), None, Some("/team"), SessionStatus::Active, false);
        let w1 = make_session("w1", Some("coder"), Some("c1"), Some("/team"), SessionStatus::Active, false);
        let s_noteam = make_session("s_noteam", None, None, Some("/team"), SessionStatus::Active, false);
        let sessions = vec![c1, w1, s_noteam.clone()];

        let workers = workers_for(&sessions, "/team");
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0].id, "w1", "Session without a team must not be included in workers");
        assert_eq!(session_workspace(&s_noteam, &sessions), None);

        let no_coord_sessions = vec![s_noteam];
        assert!(workspaces(&no_coord_sessions).is_empty(), "Session without a coordinator produces no workspaces");
    }

    #[test]
    fn update_select_workspace_and_modules() {
        let c1 = make_session("c1", Some(jgt_core::COORDINATOR), None, Some("/team1"), SessionStatus::Active, false);
        let w1 = make_session("w1", Some("coder"), Some("c1"), Some("/team1"), SessionStatus::Active, false);

        let (mut app, _) = App::new(None);
        let _ = app.update(Msg::SessionsLoaded(Ok(vec![c1, w1])));
        assert_eq!(app.workspace, None);
        assert_eq!(app.module, Module::Coordinator);

        // SelectWorkspace selects coordinator and sets module to Coordinator
        let _ = app.update(Msg::SelectWorkspace("/team1/".to_string()));
        assert_eq!(app.workspace.as_deref(), Some("/team1"));
        assert_eq!(app.selected.as_deref(), Some("c1"));
        assert_eq!(app.module, Module::Coordinator);

        // SelectModule(Board) preserves workspace and sets module to Board
        let _ = app.update(Msg::SelectModule(Module::Board));
        assert_eq!(app.module, Module::Board);
        assert_eq!(app.workspace.as_deref(), Some("/team1"));

        // SelectModule(Worker) selects worker session
        let _ = app.update(Msg::SelectModule(Module::Worker("w1".to_string())));
        assert_eq!(app.module, Module::Worker("w1".to_string()));
        assert_eq!(app.selected.as_deref(), Some("w1"));
        assert_eq!(app.workspace.as_deref(), Some("/team1"));

        // SelectModule(Coordinator) switches back to coordinator
        let _ = app.update(Msg::SelectModule(Module::Coordinator));
        assert_eq!(app.module, Module::Coordinator);
        assert_eq!(app.selected.as_deref(), Some("c1"));
    }

    #[test]
    fn session_startup_and_select_set_workspace_and_module() {
        let c1 = make_session("c1", Some(jgt_core::COORDINATOR), None, Some("/team1"), SessionStatus::Active, false);
        let w1 = make_session("w1", Some("coder"), Some("c1"), Some("/team1/.team/worktrees/jgt-25"), SessionStatus::Active, false);
        let s_noteam = make_session("s_noteam", None, None, Some("/random"), SessionStatus::Active, false);

        // --session with coordinator
        let (mut app_coord, _) = App::new(Some("c1".to_string()));
        let _ = app_coord.update(Msg::SessionsLoaded(Ok(vec![c1.clone(), w1.clone(), s_noteam.clone()])));
        assert_eq!(app_coord.selected.as_deref(), Some("c1"));
        assert_eq!(app_coord.workspace.as_deref(), Some("/team1"));
        assert_eq!(app_coord.module, Module::Coordinator);

        // --session with worker
        let (mut app_worker, _) = App::new(Some("w1".to_string()));
        let _ = app_worker.update(Msg::SessionsLoaded(Ok(vec![c1.clone(), w1.clone(), s_noteam.clone()])));
        assert_eq!(app_worker.selected.as_deref(), Some("w1"));
        assert_eq!(app_worker.workspace.as_deref(), Some("/team1"));
        assert_eq!(app_worker.module, Module::Worker("w1".to_string()));

        // Selecting a worker directly (e.g. sidebar click)
        let _ = app_coord.update(Msg::Select("w1".to_string()));
        assert_eq!(app_coord.selected.as_deref(), Some("w1"));
        assert_eq!(app_coord.workspace.as_deref(), Some("/team1"));
        assert_eq!(app_coord.module, Module::Worker("w1".to_string()));

        // Selecting a non-team session resets workspace
        let _ = app_coord.update(Msg::Select("s_noteam".to_string()));
        assert_eq!(app_coord.selected.as_deref(), Some("s_noteam"));
        assert_eq!(app_coord.workspace, None);
    }

    #[test]
    fn sessions_loaded_preserves_module_board_and_refreshes_workspace() {
        let c1 = make_session("c1", Some(jgt_core::COORDINATOR), None, Some("/team1"), SessionStatus::Active, false);
        let (mut app, _) = App::new(None);
        let _ = app.update(Msg::SessionsLoaded(Ok(vec![c1.clone()])));
        let _ = app.update(Msg::Select("c1".to_string()));
        assert_eq!(app.selected.as_deref(), Some("c1"));
        assert_eq!(app.workspace.as_deref(), Some("/team1"));
        assert_eq!(app.module, Module::Coordinator);

        // Switch to Board
        let _ = app.update(Msg::SelectModule(Module::Board));
        assert_eq!(app.module, Module::Board);

        // Sessions reload: Module::Board must survive and workspace is refreshed
        let mut c1_updated = c1.clone();
        c1_updated.cwd = Some("/team1_refreshed".to_string());
        let _ = app.update(Msg::SessionsLoaded(Ok(vec![c1_updated])));
        assert_eq!(app.module, Module::Board, "Module::Board must survive SessionsLoaded reload");
        assert_eq!(app.workspace.as_deref(), Some("/team1_refreshed"), "Workspace must be refreshed from session");

        // Reload where selected session no longer exists: module and workspace must not be cleared
        let _ = app.update(Msg::SessionsLoaded(Ok(vec![])));
        assert_eq!(app.selected, None);
        assert_eq!(app.module, Module::Board, "Module::Board must survive even if selected session vanished");
        assert_eq!(app.workspace.as_deref(), Some("/team1_refreshed"), "Workspace must not be cleared unexpectedly");
    }

    #[test]
    fn team_delete_ask_cancel_and_selection_reset() {
        let c1 = make_session("c1", Some(jgt_core::COORDINATOR), None, Some("/team1"), SessionStatus::Active, false);
        let c2 = make_session("c2", Some(jgt_core::COORDINATOR), None, Some("/team2"), SessionStatus::Active, false);

        let (mut app, _) = App::new(None);
        let _ = app.update(Msg::SessionsLoaded(Ok(vec![c1.clone(), c2.clone()])));
        let _ = app.update(Msg::SelectWorkspace("/team1".to_string()));
        assert_eq!(app.workspace.as_deref(), Some("/team1"));
        assert_eq!(app.selected.as_deref(), Some("c1"));

        // DeleteTeamAsk sets confirmation flag
        let _ = app.update(Msg::DeleteTeamAsk);
        assert!(app.confirm_delete_team);

        // DeleteTeamCancel clears confirmation flag
        let _ = app.update(Msg::DeleteTeamCancel);
        assert!(!app.confirm_delete_team);

        // TeamDeleted when another team (/team2) exists: switches to /team2
        let _ = app.update(Msg::TeamDeleted(Ok(())));
        assert_eq!(app.workspace.as_deref(), Some("/team2"));
        assert_eq!(app.selected.as_deref(), Some("c2"));

        // Update sessions so only /team2 exists, then delete /team2
        let _ = app.update(Msg::SessionsLoaded(Ok(vec![c2])));
        let _ = app.update(Msg::TeamDeleted(Ok(())));
        assert_eq!(app.workspace, None);
        assert_eq!(app.selected, None);
    }

    #[test]
    fn helpers_workers_for_orders_active_before_idle() {
        let c1 = make_session("c1", Some(jgt_core::COORDINATOR), None, Some("/team"), SessionStatus::Active, false);
        let w_idle = make_session("w_idle", Some("coder"), Some("c1"), Some("/team"), SessionStatus::Idle, false);
        let w_act1 = make_session("w_act1", Some("tester"), Some("c1"), Some("/team"), SessionStatus::Active, false);
        let w_ended = make_session("w_ended", Some("reviewer"), Some("c1"), Some("/team"), SessionStatus::Ended, false);
        let w_act2 = make_session("w_act2", Some("coder"), Some("c1"), Some("/team"), SessionStatus::Active, false);
        let w_arch = make_session("w_arch", Some("coder"), Some("c1"), Some("/team"), SessionStatus::Active, true);
        let sessions = vec![c1, w_idle, w_act1, w_ended, w_act2, w_arch];

        let workers = workers_for(&sessions, "/team");
        let ids: Vec<&str> = workers.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["w_act1", "w_act2", "w_idle", "w_ended"]);
    }

    #[test]
    fn toggle_workers_toggles_state() {
        let (mut app, _) = App::new(None);
        assert!(!app.workers_collapsed, "workers_collapsed must default to false (expanded)");

        let _ = app.update(Msg::ToggleWorkers);
        assert!(app.workers_collapsed, "ToggleWorkers must collapse workers");

        let _ = app.update(Msg::ToggleWorkers);
        assert!(!app.workers_collapsed, "ToggleWorkers must expand workers again");
    }

    #[test]
    fn module_routing_and_drafts_survive_switching() {
        let c1 = make_session("c1", Some(jgt_core::COORDINATOR), None, Some("/team1"), SessionStatus::Active, false);
        let w1 = make_session("w1", Some("coder"), Some("c1"), Some("/team1"), SessionStatus::Active, false);
        let (mut app, _) = App::new(None);
        let _ = app.update(Msg::SessionsLoaded(Ok(vec![c1, w1])));

        // Select Coordinator
        let _ = app.update(Msg::SelectWorkspace("/team1".to_string()));
        assert_eq!(app.selected.as_deref(), Some("c1"));
        assert_eq!(app.module, Module::Coordinator);

        // Type a draft in Coordinator composer
        app.composer = text_editor::Content::with_text("draft for coordinator");

        // Switch to Board module: draft is saved and module is Board
        let _ = app.update(Msg::SelectModule(Module::Board));
        assert_eq!(app.module, Module::Board);
        assert_eq!(app.drafts.get("c1").map(String::as_str), Some("draft for coordinator"));

        // Switch to Worker module: loads worker, composer gets worker's draft
        let _ = app.update(Msg::SelectModule(Module::Worker("w1".to_string())));
        assert_eq!(app.selected.as_deref(), Some("w1"));
        assert_eq!(app.module, Module::Worker("w1".to_string()));
        assert_eq!(app.composer.text().trim(), "");

        // Type draft in worker
        app.composer = text_editor::Content::with_text("draft for worker");

        // Switch back to Coordinator: coordinator's draft is restored
        let _ = app.update(Msg::SelectModule(Module::Coordinator));
        assert_eq!(app.selected.as_deref(), Some("c1"));
        assert_eq!(app.module, Module::Coordinator);
        assert_eq!(app.composer.text().trim(), "draft for coordinator");
        assert_eq!(app.drafts.get("w1").map(String::as_str), Some("draft for worker"));

        // Switch back to Worker: worker's draft is restored
        let _ = app.update(Msg::SelectModule(Module::Worker("w1".to_string())));
        assert_eq!(app.selected.as_deref(), Some("w1"));
        assert_eq!(app.module, Module::Worker("w1".to_string()));
        assert_eq!(app.composer.text().trim(), "draft for worker");
    }

    #[test]
    fn decide_thread_scroll_pure_decision() {
        // 1. Not selected session: never snaps
        assert_eq!(
            decide_thread_scroll(false, false, true, true),
            ScrollDecision::KeepPosition
        );
        assert_eq!(
            decide_thread_scroll(false, false, false, false),
            ScrollDecision::KeepPosition
        );

        // 2. Loading older history: never snaps
        assert_eq!(
            decide_thread_scroll(true, true, true, true),
            ScrollDecision::KeepPosition
        );
        assert_eq!(
            decide_thread_scroll(true, true, false, true),
            ScrollDecision::KeepPosition
        );

        // 3. Newly opened chat: always snaps to end, even if at_bottom is false
        assert_eq!(
            decide_thread_scroll(true, false, true, false),
            ScrollDecision::SnapToEnd
        );
        assert_eq!(
            decide_thread_scroll(true, false, true, true),
            ScrollDecision::SnapToEnd
        );

        // 4. Already open chat and user is at bottom: snaps to end
        assert_eq!(
            decide_thread_scroll(true, false, false, true),
            ScrollDecision::SnapToEnd
        );

        // 5. Already open chat and user has scrolled up: preserves position, does not snap
        assert_eq!(
            decide_thread_scroll(true, false, false, false),
            ScrollDecision::KeepPosition
        );
    }

    #[test]
    fn open_chat_snaps_to_end_and_preserves_scrolled_position() {
        let c1 = make_session("c1", Some(jgt_core::COORDINATOR), None, Some("/team1"), SessionStatus::Active, false);
        let w1 = make_session("w1", Some("coder"), Some("c1"), Some("/team1"), SessionStatus::Active, false);
        let (mut app, _) = App::new(None);
        let _ = app.update(Msg::SessionsLoaded(Ok(vec![c1, w1])));

        // Select workspace -> opens Coordinator chat
        let _ = app.update(Msg::SelectWorkspace("/team1".to_string()));
        assert_eq!(app.selected.as_deref(), Some("c1"));
        assert_eq!(app.module, Module::Coordinator);
        assert!(app.at_bottom, "at_bottom must be true on chat open");
        assert_eq!(app.newly_opened.as_deref(), Some("c1"), "newly_opened must be marked on open");

        // Simulate iced mounting scrollable initially at top (offset 0.0)
        app.at_bottom = false;

        // Thread finishes loading for newly opened session: snaps to end and clears newly_opened
        let m1 = test_message(1, "hello", None);
        let _ = app.update(Msg::ThreadLoaded {
            session: "c1".to_string(),
            older: false,
            result: Ok(vec![m1.clone()]),
        });
        assert!(app.at_bottom, "snap() must set at_bottom to true on initial thread load");
        assert_eq!(app.newly_opened, None, "newly_opened must be cleared after thread load");

        // User scrolls up to read earlier history
        app.at_bottom = false;

        // Background reload arrives (e.g. reconnect or sync): must NOT snap, preserving user's position
        let _ = app.update(Msg::ThreadLoaded {
            session: "c1".to_string(),
            older: false,
            result: Ok(vec![m1]),
        });
        assert!(!app.at_bottom, "Background reload must not clobber scrolled-up position");

        // Switch to Board module
        let _ = app.update(Msg::SelectModule(Module::Board));
        assert_eq!(app.module, Module::Board);

        // Switch back to Coordinator module: even though c1 is already selected, it must snap to end on open!
        let _ = app.update(Msg::SelectModule(Module::Coordinator));
        assert_eq!(app.module, Module::Coordinator);
        assert!(app.at_bottom, "Switching from Board to Coordinator must set at_bottom to true");
        assert_eq!(app.newly_opened.as_deref(), Some("c1"), "Switching from Board to Coordinator must mark newly_opened");

        // Switch to Worker module: must snap to end on open
        let _ = app.update(Msg::SelectModule(Module::Worker("w1".to_string())));
        assert_eq!(app.selected.as_deref(), Some("w1"));
        assert_eq!(app.module, Module::Worker("w1".to_string()));
        assert!(app.at_bottom, "Switching to Worker must set at_bottom to true");
        assert_eq!(app.newly_opened.as_deref(), Some("w1"), "Switching to Worker must mark newly_opened");
    }

    #[test]
    fn test_agents_locked_for() {
        let dir = std::env::temp_dir().join(format!("jgt-locked-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut expected = TeamAgents::default();
        expected.roles.insert(
            "coder".into(),
            jgt_core::AgentSetting {
                harness: Harness::Antigravity,
                model: "gemini-3.8-flash-high".into(),
                effort: "high".into(),
            },
        );
        expected.write(&dir).unwrap();

        let ws = dir.display().to_string();

        // 1. Running team: coordinator connected -> locked (Some(expected))
        let (mut app, _) = App::new(None);
        app.workspace = Some(ws.clone());
        app.sessions = vec![make_session(
            "c1",
            Some(jgt_core::COORDINATOR),
            None,
            Some(&ws),
            SessionStatus::Active,
            false,
        )];
        assert_eq!(agents_locked_for(&app), Some(expected.clone()));

        // Also test Listening status (connected)
        app.sessions[0].status = SessionStatus::Listening;
        assert_eq!(agents_locked_for(&app), Some(expected));

        // 2. Ended team: coordinator not connected -> editable (None)
        app.sessions[0].status = SessionStatus::Ended;
        assert_eq!(agents_locked_for(&app), None);

        // 3. No workspace: None -> editable (None)
        app.workspace = None;
        app.sessions[0].status = SessionStatus::Active;
        assert_eq!(agents_locked_for(&app), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    static CONFIG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_save_agents_keeps_other_config() {
        let _guard = CONFIG_LOCK.lock().unwrap();

        let dir = std::env::temp_dir().join(format!("jgt-save-agents-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let initial_toml = r#"
[desktop]
theme = "dark"
style = "dracula"
zoom = 1.2
text_size = 14

[notifications]
enabled = false

[team.skills.coder]
harness = "claude-code"
model = "haiku"
effort = "low"
allowed_tools = ["Bash"]
"#;
        std::fs::write(dir.join("config.toml"), initial_toml).unwrap();

        let prev = std::env::var_os("JGT_CONFIG_DIR");
        unsafe {
            std::env::set_var("JGT_CONFIG_DIR", &dir);
        }

        let (mut app, _) = App::new(None);
        app.agents = vec![
            AgentRow {
                role: "coder".into(),
                harness: Harness::Antigravity,
                model: "gemini-3.8-flash-high".into(),
                effort: "high".into(),
            },
            AgentRow {
                role: "coordinator".into(),
                harness: Harness::ClaudeCode,
                model: "opus".into(),
                effort: "medium".into(),
            },
        ];

        let _ = app.update(Msg::SaveAgents);

        assert_eq!(
            app.toast.as_ref().map(|t| (t.text.as_str(), t.error)),
            Some(("Applies to teams started from now on", false))
        );

        let saved = Config::load();
        // Desktop config is preserved
        assert_eq!(saved.desktop.style, "dracula");
        assert_eq!(saved.desktop.zoom, 1.2);
        assert_eq!(saved.desktop.text_size, 14);
        assert_eq!(saved.desktop.theme, "dark");
        assert!(!saved.notifications.enabled);

        // Coder settings updated, keeping allowed_tools intact
        let coder = saved.team.skills.get("coder").expect("coder skill must exist");
        assert_eq!(coder.harness.as_deref(), Some("antigravity"));
        assert_eq!(coder.model.as_deref(), Some("gemini-3.8-flash-high"));
        assert_eq!(coder.effort.as_deref(), Some("high"));
        assert_eq!(coder.allowed_tools, Some(vec!["Bash".into()]));

        // Coordinator settings written
        let coord = saved.team.skills.get("coordinator").expect("coordinator skill must exist");
        assert_eq!(coord.harness.as_deref(), Some("claude-code"));
        assert_eq!(coord.model.as_deref(), Some("opus"));
        assert_eq!(coord.effort.as_deref(), Some("medium"));

        match prev {
            Some(v) => unsafe { std::env::set_var("JGT_CONFIG_DIR", v) },
            None => unsafe { std::env::remove_var("JGT_CONFIG_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_set_agent_harness_and_model_messages() {
        let (mut app, _) = App::new(None);
        app.harness_models.insert(
            Harness::Antigravity,
            Ok(vec!["gemini-3.8-flash-high".into(), "gemini-1.5-pro".into()]),
        );
        app.agents = vec![
            AgentRow {
                role: "coder".into(),
                harness: Harness::ClaudeCode,
                model: "sonnet".into(),
                effort: "high".into(),
            },
            AgentRow {
                role: "coordinator".into(),
                harness: Harness::ClaudeCode,
                model: "opus".into(),
                effort: "medium".into(),
            },
        ];

        // SetAgentHarness on worker: changes harness and resets model to first suggestion
        let _ = app.update(Msg::SetAgentHarness("coder".into(), Harness::Antigravity));
        assert_eq!(app.agents[0].harness, Harness::Antigravity);
        assert_eq!(app.agents[0].model, "gemini-3.8-flash-high");
        assert_eq!(app.agents[0].effort, "high"); // effort kept

        // SetAgentHarness on coordinator: ignored!
        let _ = app.update(Msg::SetAgentHarness("coordinator".into(), Harness::Antigravity));
        assert_eq!(app.agents[1].harness, Harness::ClaudeCode);
        assert_eq!(app.agents[1].model, "opus");

        // SetAgentModel on worker: updates model
        let _ = app.update(Msg::SetAgentModel("coder".into(), "custom-gemini".into()));
        assert_eq!(app.agents[0].model, "custom-gemini");

        // SetAgentModel on coordinator: updates model
        let _ = app.update(Msg::SetAgentModel("coordinator".into(), "fable".into()));
        assert_eq!(app.agents[1].model, "fable");

        // ModelsListed updates harness_models
        let _ = app.update(Msg::ModelsListed(Harness::Opencode, Ok(vec!["anthropic/claude-3".into()])));
        assert_eq!(
            app.harness_models.get(&Harness::Opencode),
            Some(&Ok(vec!["anthropic/claude-3".into()]))
        );

        // SetAgentEffort on worker: updates effort
        let _ = app.update(Msg::SetAgentEffort("coder".into(), "xhigh".into()));
        assert_eq!(app.agents[0].effort, "xhigh");

        // SetAgentEffort on coordinator: updates effort
        let _ = app.update(Msg::SetAgentEffort("coordinator".into(), "low".into()));
        assert_eq!(app.agents[1].effort, "low");

        // Harness without levels (Opencode) keeps effort intact
        let _ = app.update(Msg::SetAgentHarness("coder".into(), Harness::Opencode));
        assert_eq!(app.agents[0].harness, Harness::Opencode);
        assert_eq!(app.agents[0].effort, "xhigh");

        // Harness switch clamps effort when switching to a harness with levels and effort is not one of them
        app.agents[0].effort = "unsupported".into();
        let _ = app.update(Msg::SetAgentHarness("coder".into(), Harness::Antigravity));
        assert_eq!(app.agents[0].harness, Harness::Antigravity);
        assert_eq!(app.agents[0].effort, "low");
    }

    #[test]
    fn test_apply_agents_writes_effort() {
        let mut config = Config::default();
        let agents = vec![
            AgentRow {
                role: "coder".into(),
                harness: Harness::Antigravity,
                model: "gemini-3.8-flash-high".into(),
                effort: "xhigh".into(),
            },
            AgentRow {
                role: "coordinator".into(),
                harness: Harness::ClaudeCode,
                model: "opus".into(),
                effort: "low".into(),
            },
        ];
        apply_agents(&mut config, &agents);
        assert_eq!(
            config.team.skills.get("coder").and_then(|s| s.effort.as_deref()),
            Some("xhigh")
        );
        assert_eq!(
            config.team.skills.get("coordinator").and_then(|s| s.effort.as_deref()),
            Some("low")
        );
    }

    #[test]
    fn test_toggle_settings_populates_agents() {
        let (mut app, _) = App::new(None);
        assert!(!app.show_settings);
        assert!(app.agents.is_empty());

        let _ = app.update(Msg::ToggleSettings);
        assert!(app.show_settings);
        assert_eq!(app.agents.len(), 5);
        let roles: Vec<&str> = app.agents.iter().map(|a| a.role.as_str()).collect();
        assert_eq!(
            roles,
            vec!["coordinator", "planner", "coder", "reviewer", "git-coder"]
        );
        // Coordinator harness is always Claude Code
        assert_eq!(app.agents[0].harness, Harness::ClaudeCode);

        // Toggle closed
        let _ = app.update(Msg::ToggleSettings);
        assert!(!app.show_settings);
    }

    #[test]
    fn test_toggle_settings_with_ended_team_uses_user_config() {
        let _guard = CONFIG_LOCK.lock().unwrap();

        let base_dir = std::env::temp_dir().join(format!("jgt-ended-test-{}", std::process::id()));
        let config_dir = base_dir.join("config");
        let ws_dir = base_dir.join("workspace");
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(&ws_dir).unwrap();

        // 1. User config: coder uses claude-code with haiku
        let user_toml = r#"
[team.skills.coder]
harness = "claude-code"
model = "haiku"
effort = "low"
"#;
        std::fs::write(config_dir.join("config.toml"), user_toml).unwrap();

        let prev = std::env::var_os("JGT_CONFIG_DIR");
        unsafe {
            std::env::set_var("JGT_CONFIG_DIR", &config_dir);
        }

        // 2. Workspace has saved agents.json from a team
        let mut saved_agents = TeamAgents::default();
        saved_agents.roles.insert(
            "coder".into(),
            jgt_core::AgentSetting {
                harness: Harness::Antigravity,
                model: "gemini-3.8-flash-high".into(),
                effort: "high".into(),
            },
        );
        saved_agents.write(&ws_dir).unwrap();

        // Workspace also has project .team/config.toml
        std::fs::write(
            ws_dir.join(".team").join("config.toml"),
            "[team.skills.coder]\nmodel = \"project-model\"\n",
        )
        .unwrap();

        // 3. App has this workspace selected and an ended coordinator session
        let (mut app, _) = App::new(None);
        let ws = ws_dir.display().to_string();
        app.workspace = Some(ws.clone());
        app.sessions = vec![make_session(
            "c1",
            Some(jgt_core::COORDINATOR),
            None,
            Some(&ws),
            SessionStatus::Ended,
            false,
        )];

        // 4. ToggleSettings: Settings opens in editable (unlocked) mode
        let _ = app.update(Msg::ToggleSettings);
        assert!(app.show_settings);
        assert!(app.agents_locked.is_none());

        // 5. The rows equal the user config alone
        let config = Config::load();
        let expected_agents: Vec<AgentRow> = jgt_core::config::roles()
            .map(|role| {
                let s = config.team.for_user(role);
                AgentRow {
                    role: role.to_string(),
                    harness: s.harness,
                    model: s.model,
                    effort: s.effort,
                }
            })
            .collect();
        assert_eq!(app.agents, expected_agents);

        let coder_row = app.agents.iter().find(|a| a.role == "coder").unwrap();
        assert_eq!(coder_row.harness, Harness::ClaudeCode);
        assert_eq!(coder_row.model, "haiku");
        assert_eq!(coder_row.effort, "low");

        // Coordinator is Claude Code
        let coord_row = app.agents.iter().find(|a| a.role == "coordinator").unwrap();
        assert_eq!(coord_row.harness, Harness::ClaudeCode);

        // 6. Contrast: when team is running (Active coordinator), Settings opens locked with agents.json
        let _ = app.update(Msg::ToggleSettings); // close
        assert!(!app.show_settings);

        app.sessions[0].status = SessionStatus::Active;
        let _ = app.update(Msg::ToggleSettings); // open again
        assert!(app.show_settings);
        assert!(app.agents_locked.is_some());
        let coder_locked = app.agents.iter().find(|a| a.role == "coder").unwrap();
        assert_eq!(coder_locked.harness, Harness::Antigravity);
        assert_eq!(coder_locked.model, "gemini-3.8-flash-high");

        match prev {
            Some(v) => unsafe { std::env::set_var("JGT_CONFIG_DIR", v) },
            None => unsafe { std::env::remove_var("JGT_CONFIG_DIR") },
        }
        let _ = std::fs::remove_dir_all(&base_dir);
    }
}

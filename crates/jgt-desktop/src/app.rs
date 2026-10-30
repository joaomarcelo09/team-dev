//! Application state and update logic (Elm style: `Msg` in, state change + `Task` out).

use crate::hub::{self, Feed};
use iced::{
    Subscription, Task, Theme,
    theme,
    widget::{markdown, operation, scrollable, text_editor},
    keyboard, window,
};
use jgt_core::{
    Decision, Direction, Event, Message, MessageKind, Question, ReconnectMode, Session, SessionStatus, Task as BoardTask,
    TaskEvent, TaskPriority, TaskStatus,
    api::{MoveResponse, PatchSessionRequest},
    config::{Config, Desktop as DesktopConfig},
};
use std::{
    collections::HashMap,
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
    fn upsert(&mut self, message: Message) {
        self.markdown.insert(message.id, crate::view::parse_markdown(&message.body));
        self.editors.insert(message.id, text_editor::Content::with_text(&message.body));
        match self.messages.binary_search_by_key(&message.id, |m| m.id) {
            Ok(i) => self.messages[i] = message,
            Err(i) => self.messages.insert(i, message),
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

/// What a Coordinator's chat shows: the conversation or its workspace's Board.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Chat,
    Board,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemePref {
    System,
    Light,
    Dark,
}

pub struct App {
    pub conn: Conn,
    pub sessions: Vec<Session>,
    pub selected: Option<String>,
    pub threads: HashMap<String, Thread>,
    pub composer: text_editor::Content,
    /// Unsent text per session, kept while switching chats.
    pub drafts: HashMap<String, String>,
    /// Free-text answers being typed for questions.
    pub answers: HashMap<String, String>,
    pub search: String,
    pub show_archived: bool,
    pub rename: Option<String>,
    pub confirm_delete: bool,
    pub reconnecting: bool,
    pub toast: Option<Toast>,
    pub focused: bool,
    pub window: Option<window::Id>,
    pub at_bottom: bool,
    pub theme_pref: ThemePref,
    pub system_mode: theme::Mode,
    /// Whole-window scale (1.0 = 100%).
    pub zoom: f32,
    /// Chat text size in pixels.
    pub text_size: u16,
    pub popups: bool,
    pub show_settings: bool,
    /// Problems in the `[team]` config (unknown harness or effort, …), shown as a banner in Settings.
    pub config_warnings: Vec<String>,
    /// Folder typed in the "New team" form, while it is open.
    pub new_team: Option<String>,
    pub starting_team: bool,
    /// Chat or Board per Coordinator session; Chat when absent.
    pub tabs: HashMap<String, Tab>,
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
    SetTab(Tab),
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
    Search(String),
    ToggleArchived,
    Copy(String),
    Link(markdown::Uri),
    Window(window::Id, window::Event),
    SystemTheme(theme::Mode),
    SetTheme(ThemePref),
    ToggleSettings,
    TogglePopups,
    Zoom(Step),
    TextSize(Step),
    Key(keyboard::Event),
    ToggleSelect(i64),
    MessageEditor(i64, text_editor::Action),
    DismissToast,
    Tick,
    Done(Result<(), String>),
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
        let app = App {
            conn: Conn::Connecting,
            sessions: Vec::new(),
            selected: None,
            threads: HashMap::new(),
            composer: text_editor::Content::new(),
            drafts: HashMap::new(),
            answers: HashMap::new(),
            search: String::new(),
            show_archived: false,
            rename: None,
            confirm_delete: false,
            reconnecting: false,
            toast: None,
            focused: true,
            window: None,
            at_bottom: true,
            theme_pref,
            system_mode: theme::Mode::Dark,
            zoom: clamp_zoom(desktop.zoom),
            text_size,
            popups: config.notifications.enabled,
            show_settings: false,
            config_warnings: config.team.warnings(None),
            new_team: None,
            starting_team: false,
            tabs: HashMap::new(),
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
        let dark = match self.theme_pref {
            ThemePref::Dark => true,
            ThemePref::Light => false,
            ThemePref::System => self.system_mode != theme::Mode::Light,
        };
        if dark { crate::style::dark() } else { crate::style::light() }
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

    fn toast(&mut self, text: impl Into<String>, error: bool) {
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
        if self.selected.as_deref() == Some(id.as_str()) {
            return Task::none();
        }
        if let Some(prev) = self.selected.take() {
            self.drafts.insert(prev, self.composer.text());
        }
        let draft = self.drafts.remove(&id).unwrap_or_default();
        self.composer = text_editor::Content::with_text(draft.trim_end_matches('\n'));
        self.selected = Some(id.clone());
        self.rename = None;
        self.confirm_delete = false;
        self.close_task();
        self.new_task = None;
        self.selecting_message = None;
        Task::batch([
            self.load_selected_board(),
            self.load_thread(id.clone()),
            self.mark_read_if_visible(&id),
            self.presence(),
            operation::focus("composer"),
        ])
    }

    /// The tab a session shows (only Coordinators have a Board).
    pub fn tab(&self, session_id: &str) -> Tab {
        self.tabs.get(session_id).copied().unwrap_or_default()
    }

    fn load_board(&self, workspace: String) -> Task<Msg> {
        let w = workspace.clone();
        Task::perform(hub::tasks(workspace), move |result| Msg::BoardLoaded { workspace: w.clone(), result })
    }

    /// Loads the selected Coordinator's Board when its Board tab is showing.
    fn load_selected_board(&self) -> Task<Msg> {
        match self.selected_session() {
            Some(s) if self.tab(&s.id) == Tab::Board => match crate::board::workspace(s) {
                Some(w) => self.load_board(w),
                None => Task::none(),
            },
            _ => Task::none(),
        }
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
                    _ => Task::none(),
                }
            }
            Msg::SessionsLoaded(Err(e)) => {
                self.toast(format!("Could not load sessions: {e}"), true);
                Task::none()
            }
            Msg::Select(id) => self.select(id),
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
                    if !older && self.selected.as_deref() == Some(session.as_str()) {
                        return self.snap();
                    }
                    Task::none()
                }
                Err(e) => {
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
            Msg::SetTab(tab) => {
                let Some(id) = self.selected.clone() else { return Task::none() };
                self.tabs.insert(id, tab);
                match tab {
                    Tab::Board => self.load_selected_board(),
                    Tab::Chat => {
                        self.close_task();
                        self.new_task = None;
                        self.snap()
                    }
                }
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
                let Some(workspace) = self.selected_session().and_then(crate::board::workspace) else { return Task::none() };
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
                let Some(workspace) = self.selected_session().and_then(crate::board::workspace) else { return Task::none() };
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
            Msg::Done(Ok(())) => Task::none(),
            Msg::Search(text) => {
                self.search = text;
                Task::none()
            }
            Msg::ToggleArchived => {
                self.show_archived = !self.show_archived;
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
            Msg::ToggleSettings => {
                self.show_settings = !self.show_settings;
                if self.show_settings {
                    let config = Config::load();
                    self.popups = config.notifications.enabled;
                    self.config_warnings = config.team.warnings(None);
                }
                Task::none()
            }
            Msg::TogglePopups => {
                self.popups = !self.popups;
                let mut config = Config::load();
                apply_popups(&mut config, self.popups);
                let _ = config.save();
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

    /// Saves theme, zoom and text size to config.toml (other settings there are kept).
    fn save_view_settings(&self) {
        let mut config = Config::load();
        config.desktop.theme = match self.theme_pref {
            ThemePref::System => "system",
            ThemePref::Dark => "dark",
            ThemePref::Light => "light",
        }
        .into();
        config.desktop.zoom = self.zoom;
        config.desktop.text_size = self.text_size;
        let _ = config.save();
    }
}

/// Applies the desktop pop-ups setting to a config.
pub(crate) fn apply_popups(config: &mut Config, enabled: bool) {
    config.notifications.enabled = enabled;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applying_popups_toggle_updates_config() {
        let mut config = Config::default();
        assert!(config.notifications.enabled);
        apply_popups(&mut config, false);
        assert!(!config.notifications.enabled);
        apply_popups(&mut config, true);
        assert!(config.notifications.enabled);
    }
}

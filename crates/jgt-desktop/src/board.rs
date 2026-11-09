//! The Board tab of a Coordinator's chat: six columns of task cards and the task side panel.
//! The pure helpers (column order, Done limit, labels) sit at the top and are unit-tested.

use crate::{
    app::{App, Msg},
    icons, style,
    view::{bold, clock, dot, muted, short_time, tip},
};
use iced::{
    Alignment, Color, Element, Fill, Padding, Theme, mouse,
    widget::{
        Space, button, column, container, markdown, mouse_area, pick_list, row, rule, scrollable, space, text,
        text_editor, text_input,
    },
};
use jgt_core::{
    Actor, Finding, ReviewDecision, Session, Severity, Task, TaskEvent, TaskEventKind, TaskPriority, TaskStatus,
    TestStatus, WaitingKind,
};

/// Id of the "+" form's title input.
pub const NEW_TASK_ID: &str = "new-task";
/// Done shows this many tasks, newest first, until "Show all".
pub const DONE_SHOWN: usize = 20;

// ── helpers ─────────────────────────────────────────────────────────────────

/// The Board a session shows: a Coordinator's folder, without a trailing slash as the hub keeps it.
pub fn workspace(s: &Session) -> Option<String> {
    if !s.is_coordinator() {
        return None;
    }
    let cwd = s.cwd.as_deref()?.trim();
    match cwd.trim_end_matches('/') {
        "" if cwd.starts_with('/') => Some("/".into()),
        "" => None,
        w => Some(w.to_string()),
    }
}

fn order_key(t: &Task) -> (usize, i64, i64) {
    (t.status.index(), t.position, t.number)
}

/// A Board as the app holds it: its tasks in column then position order, each column numbered
/// from 0 without gaps as the hub numbers it, and Done's newest-first order, worked out once per
/// change rather than every frame.
#[derive(Debug, Clone, Default)]
pub struct Board {
    tasks: Vec<Task>,
    /// Indices into `tasks` of the Done cards, newest first.
    done: Vec<usize>,
}

impl Board {
    /// A Board as the hub lists it (archived tasks are not shown).
    pub fn new(mut tasks: Vec<Task>) -> Self {
        tasks.retain(|t| !t.archived);
        tasks.sort_by_key(order_key);
        let mut board = Board { tasks, done: Vec::new() };
        for status in TaskStatus::ALL.iter().copied() {
            board.renumber(status);
        }
        board.changed();
        board
    }

    /// A Board loaded from the hub, keeping the copies `old` holds of tasks changed since.
    pub fn reload(old: Option<&Board>, mut tasks: Vec<Task>) -> Self {
        if let Some(old) = old {
            for t in &mut tasks {
                if let Some(held) = old.get(&t.id).filter(|held| held.updated_at > t.updated_at) {
                    *t = held.clone();
                }
            }
        }
        Board::new(tasks)
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    pub fn get(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    fn range(&self, status: TaskStatus) -> std::ops::Range<usize> {
        let i = status.index();
        self.tasks.partition_point(|t| t.status.index() < i)..self.tasks.partition_point(|t| t.status.index() <= i)
    }

    /// The cards of one column, top to bottom (Done by position; it shows `done`).
    pub fn column(&self, status: TaskStatus) -> &[Task] {
        &self.tasks[self.range(status)]
    }

    /// The Done cards, newest first.
    pub fn done(&self) -> impl Iterator<Item = &Task> {
        self.done.iter().map(|&i| &self.tasks[i])
    }

    fn renumber(&mut self, status: TaskStatus) {
        let range = self.range(status);
        for (i, t) in self.tasks[range].iter_mut().enumerate() {
            t.position = i as i64;
        }
    }

    fn changed(&mut self) {
        self.done = self.range(TaskStatus::Done).collect();
        let tasks = &self.tasks;
        self.done.sort_by_key(|&i| {
            let t = &tasks[i];
            (std::cmp::Reverse(t.done_at.unwrap_or(t.updated_at)), std::cmp::Reverse(t.number))
        });
    }

    /// Takes a task out and closes the gap in its column, as the hub does.
    fn take(&mut self, id: &str) -> bool {
        let Some(i) = self.tasks.iter().position(|t| t.id == id) else { return false };
        let old = self.tasks.remove(i);
        self.renumber(old.status);
        true
    }

    /// Puts a new version of `task` on the Board the way the hub's `place` does: out of its old
    /// column, which closes the gap, then at index `position` of its column, which is numbered
    /// from 0 again. The hub only sends the moved task, not the neighbours it renumbered, so this
    /// keeps their order right. An archived task leaves the Board. A version older than the one
    /// held is ignored (a late reply to the app's own write); returns whether it was taken.
    pub fn upsert(&mut self, task: Task) -> bool {
        if self.get(&task.id).is_some_and(|held| held.updated_at > task.updated_at) {
            return false;
        }
        self.take(&task.id);
        if !task.archived {
            let range = self.range(task.status);
            let at = range.start + usize::try_from(task.position).unwrap_or(0).min(range.len());
            let status = task.status;
            self.tasks.insert(at, task);
            self.renumber(status);
        }
        self.changed();
        true
    }

    /// Takes a deleted task off the Board.
    pub fn remove(&mut self, id: &str) {
        if self.take(id) {
            self.changed();
        }
    }
}

/// A card being dragged (K-4): no hub call until it is dropped.
#[derive(Debug, Clone, PartialEq)]
pub struct Drag {
    pub id: String,
    pub over: Option<Target>,
}

/// Where a dragged card would land.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Target {
    /// Above the card at this index of the column.
    Before(TaskStatus, usize),
    /// At the bottom of the column.
    End(TaskStatus),
}

impl Target {
    fn status(self) -> TaskStatus {
        match self {
            Target::Before(s, _) | Target::End(s) => s,
        }
    }
}

/// The move a drop on `target` makes: the column and the `position` the hub expects (the index
/// among the column's other cards). `None` when the card would land where it is (a click).
/// Done is ordered by date, so a drop there goes to its end.
pub fn drop_move(board: &Board, id: &str, target: Target) -> Option<(TaskStatus, Option<i64>)> {
    let task = board.get(id)?;
    let to = target.status();
    let column = board.column(task.status);
    let at = column.iter().position(|t| t.id == id)?;
    if to != task.status {
        return Some(match target {
            Target::Before(_, i) if to != TaskStatus::Done => (to, Some(i as i64)),
            _ => (to, None),
        });
    }
    let i = match target {
        _ if to == TaskStatus::Done => return None,
        Target::Before(_, i) => i,
        Target::End(_) => column.len(),
    };
    // Without the card itself, the cards below it move up by one.
    let i = if i > at { i - 1 } else { i };
    (i != at).then_some((to, Some(i as i64)))
}

/// Where a card's reorder button aims it: Top (`Some(0)`) when not first, Bottom (`None`) when first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReorderTarget {
    Top,
    Bottom,
}

impl ReorderTarget {
    /// The position index expected by the hub: `Some(0)` for top, `None` for bottom (end of column).
    pub const fn position(self) -> Option<i64> {
        match self {
            ReorderTarget::Top => Some(0),
            ReorderTarget::Bottom => None,
        }
    }

    /// The icon for this reorder action.
    pub const fn icon(self) -> icons::Icon {
        match self {
            ReorderTarget::Top => icons::Icon::ArrowUpward,
            ReorderTarget::Bottom => icons::Icon::ArrowDownward,
        }
    }

    /// Tooltip explaining the action.
    pub const fn tooltip(self) -> &'static str {
        match self {
            ReorderTarget::Top => "Send to top",
            ReorderTarget::Bottom => "Send to bottom",
        }
    }
}

/// Decides whether a card moves to the top or bottom of its column:
/// None when the card cannot move within its column (e.g. single card in the column or invalid index);
/// Bottom when the card is currently first (index == 0);
/// Top when the card is not first (index > 0).
pub fn reorder_target(index: usize, len: usize) -> Option<ReorderTarget> {
    if len <= 1 || index >= len {
        return None;
    }
    Some(if index == 0 { ReorderTarget::Bottom } else { ReorderTarget::Top })
}

/// The move message produced by the reorder button.
/// None when the card cannot move within its column (e.g. single card or in Done).
pub fn reorder_msg(id: &str, status: TaskStatus, index: usize, len: usize) -> Option<Msg> {
    if status == TaskStatus::Done {
        return None;
    }
    let target = reorder_target(index, len)?;
    Some(Msg::MoveTask {
        id: id.to_string(),
        to: status,
        position: target.position(),
    })
}

/// What Done shows of its `count` cards: the 20 newest unless `show_all`, and how many are hidden.
pub fn done_shown(count: usize, show_all: bool) -> (usize, usize) {
    if show_all || count <= DONE_SHOWN { (count, 0) } else { (DONE_SHOWN, count - DONE_SHOWN) }
}

/// `round 2/3`, the review cycle the task is in (as `task list` shows it), while it matters.
pub fn round_label(task: &Task) -> Option<String> {
    task.shows_round().then(|| {
        let (round, limit) = task.round();
        format!("round {round}/{limit}")
    })
}

/// A session by hub id or by the agent's own (external) id.
fn session_by<'a>(sessions: &'a [Session], id: &str) -> Option<&'a Session> {
    sessions.iter().find(|s| s.id == id || s.external_id == id)
}

/// How the activity log names who did something: "you", "Coordinator", "tester", ….
pub fn actor_label(actor: &Actor, sessions: &[Session]) -> String {
    match actor {
        Actor::User => "you".into(),
        Actor::Coordinator { .. } => "Coordinator".into(),
        Actor::Worker { session } => match session_by(sessions, session).and_then(|s| s.role.clone()) {
            Some(role) => role,
            None => actor.label(),
        },
        Actor::Hub => "hub".into(),
    }
}

/// `↳ tester`: the worker on the card right now (K14).
pub fn worker_label(task: &Task, sessions: &[Session]) -> Option<String> {
    let worker = task.current_worker.as_deref()?;
    let name = session_by(sessions, worker).and_then(|s| s.role.clone()).unwrap_or_else(|| jgt_core::short_id(worker).to_string());
    Some(format!("↳ {name}"))
}

/// The text of an editor without the newline the editor keeps at the end.
pub fn editor_text(content: &text_editor::Content) -> String {
    content.text().trim_end_matches('\n').to_string()
}

/// One line for an activity log row.
pub fn describe(e: &TaskEvent) -> String {
    let field = |k: &str| e.meta.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let column = |k: &str| field(k).parse::<TaskStatus>().map(|s| s.label().to_string()).unwrap_or_else(|_| field(k));
    let text = match e.kind {
        TaskEventKind::Moved => {
            let mut line = format!("moved {} → {}", column("from"), column("to"));
            if !e.body.is_empty() {
                line.push_str(&format!(" ({})", e.body));
            }
            if let Some(gate) = e.meta.get("gate_skipped").and_then(|v| v.as_str()) {
                line.push_str(&format!(" · gate skipped: {gate}"));
            }
            return line;
        }
        TaskEventKind::Created => return format!("created in {}", column("status")),
        _ => e.body.trim().to_string(),
    };
    let kind = e.kind.as_str().replace('_', " ");
    if text.is_empty() { kind } else { format!("{kind}: {text}") }
}

/// A column choice for the "Move to…" list.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Column(TaskStatus);

/// The "Move to…" choices of a card in each column: every other column.
static MOVE_TO: [[Column; 5]; 6] = [others(0), others(1), others(2), others(3), others(4), others(5)];

const fn others(skip: usize) -> [Column; 5] {
    use TaskStatus::*;
    let all = [Backlog, OnQueue, Working, Testing, Review, Done];
    let mut out = [Column(Backlog); 5];
    let (mut i, mut j) = (0, 0);
    while i < all.len() {
        if i != skip {
            out[j] = Column(all[i]);
            j += 1;
        }
        i += 1;
    }
    out
}

impl std::fmt::Display for Column {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.label())
    }
}

// ── view ────────────────────────────────────────────────────────────────────

fn column_width() -> f32 {
    style::fs(14) * 17.0
}

pub fn board<'a>(app: &'a App, workspace: &str, theme: &Theme) -> Element<'a, Msg> {
    let Some(tasks) = app.boards.get(workspace) else {
        return container(muted("Loading the Board…")).center(Fill).into();
    };
    let mut lanes = row![].spacing(12).padding(Padding { top: 6.0, right: 16.0, bottom: 14.0, left: 16.0 }).height(Fill);
    for status in TaskStatus::ALL.iter().copied() {
        lanes = lanes.push(lane(app, tasks, status, theme));
    }
    let columns = scrollable(lanes)
        .direction(scrollable::Direction::Horizontal(scrollable::Scrollbar::default()))
        .width(Fill)
        .height(Fill);
    let mut bar = row![].spacing(8).padding([8, 16]).align_y(Alignment::Center);
    if let Some(folder) = app.offer_team.as_ref().filter(|w| *w == workspace) {
        bar = bar.push(
            container(
                row![
                    text("No Coordinator is running for this folder, so nobody takes the queue. Start the team?")
                        .size(style::fs(12)),
                    button(text(if app.starting_team { "Starting…" } else { "Start team" }).size(style::fs(12)))
                        .on_press_maybe((!app.starting_team).then(|| Msg::StartTeamFor(folder.clone())))
                        .style(style::primary_button)
                        .padding([3, 10]),
                    button(text("Not now").size(style::fs(12))).on_press(Msg::DismissTeamOffer).style(style::ghost).padding([3, 8]),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .padding([4, 10])
            .style(style::banner(theme.extended_palette().warning.base.color)),
        );
    }
    bar = bar.push(space::horizontal()).push(tip(
        button(text("Export").size(style::fs(12))).on_press(Msg::ExportBoard).style(style::pill_button).padding([3, 10]),
        jgt_core::board::EXPORT_PATH,
    ));
    let board = column![bar, columns].height(Fill);
    // A release outside every card and column, or leaving the Board, drops nothing.
    let board = if app.drag.is_some() {
        Element::from(mouse_area(board).on_release(Msg::DragEnd).on_exit(Msg::DragEnd).interaction(mouse::Interaction::Grabbing))
    } else {
        board.into()
    };
    // Always a row, so opening the panel keeps the columns' scroll position.
    row![board].push(app.open_task().map(|task| task_panel(app, task, theme))).height(Fill).into()
}

/// While dragging, a drop target: hovering it aims the card there, releasing on it drops.
fn target<'a>(app: &App, content: impl Into<Element<'a, Msg>>, at: Target) -> Element<'a, Msg> {
    if app.drag.is_some() { mouse_area(content).on_enter(Msg::DragOver(at)).on_release(Msg::Drop).into() } else { content.into() }
}

/// Where a dragged card lands, shown between the cards.
fn drop_line<'a>() -> Element<'a, Msg> {
    container(Space::new()).height(3).width(Fill).style(style::drop_line).into()
}

fn lane<'a>(app: &'a App, board: &'a Board, status: TaskStatus, theme: &Theme) -> Element<'a, Msg> {
    let cards = board.column(status);
    let count = cards.len();
    let mut header = row![
        text(status.label()).size(style::fs(13)).font(bold()),
        muted(format!("· {count}")).size(style::fs(12)),
        space::horizontal(),
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    let can_add = matches!(status, TaskStatus::Backlog | TaskStatus::OnQueue);
    if can_add {
        header = header.push(tip(
            button(text("+").size(style::fs(15)).font(bold())).on_press(Msg::NewTaskOpen(status)).style(style::ghost).padding([0, 8]),
            "New task",
        ));
    }

    let aimed = app.drag.as_ref().and_then(|d| d.over);
    // Done is ordered by date: every drop there goes to its end.
    let at = |i: usize| if status == TaskStatus::Done { Target::End(status) } else { Target::Before(status, i) };
    let mut list = column![].spacing(8);
    if let Some((_, title)) = app.new_task.as_ref().filter(|(c, _)| *c == status) {
        let can_save = !title.trim().is_empty();
        list = list.push(
            container(
                column![
                    text_input("Title", title)
                        .id(NEW_TASK_ID)
                        .on_input(Msg::NewTaskInput)
                        .on_submit(Msg::NewTaskSave)
                        .size(style::fs(13))
                        .padding([6, 8])
                        .style(style::input),
                    row![
                        space::horizontal(),
                        button(text("Cancel").size(style::fs(12))).on_press(Msg::NewTaskCancel).style(style::ghost).padding([4, 8]),
                        button(text("Add").size(style::fs(12)))
                            .on_press_maybe(can_save.then_some(Msg::NewTaskSave))
                            .style(style::primary_button)
                            .padding([4, 10]),
                    ]
                    .spacing(6),
                ]
                .spacing(6),
            )
            .padding(8)
            .style(style::task_card(true, false)),
        );
    }
    if count == 0 && app.new_task.as_ref().is_none_or(|(c, _)| *c != status) {
        list = list.push(container(muted("No tasks").size(style::fs(12))).padding([6, 4]));
    }
    let hidden = if status == TaskStatus::Done {
        let (shown, hidden) = done_shown(count, app.show_all_done);
        for (i, task) in board.done().take(shown).enumerate() {
            list = list.push(card(app, task, i, count, at(0), theme));
        }
        hidden
    } else {
        for (i, task) in cards.iter().enumerate() {
            if aimed == Some(Target::Before(status, i)) {
                list = list.push(drop_line());
            }
            list = list.push(card(app, task, i, count, at(i), theme));
        }
        0
    };
    if app.drag.is_some() {
        // Room under the cards to drop at the end of the column.
        let end = column![].push((aimed == Some(Target::End(status))).then(drop_line)).height(style::fs(14) * 4.0).width(Fill);
        list = list.push(target(app, end, Target::End(status)));
    }
    if status == TaskStatus::Done && (hidden > 0 || app.show_all_done) && count > DONE_SHOWN {
        let (label, on) = if app.show_all_done { ("Show fewer".to_string(), false) } else { (format!("Show all ({hidden} more)"), true) };
        list = list.push(
            container(button(text(label).size(style::fs(12))).on_press(Msg::ShowAllDone(on)).style(style::pill_button).padding([4, 10]))
                .center_x(Fill),
        );
    }
    container(
        column![
            // A fixed height, so headers with and without "+" line up.
            target(app, container(header).padding([0, 10]).center_y(style::fs(14) * 2.6), at(0)),
            scrollable(container(list).padding([0, 8]).width(Fill)).height(Fill).width(Fill),
        ]
        .spacing(2),
    )
    .width(column_width())
    .height(Fill)
    .style(style::board_column)
    .into()
}

fn card<'a>(app: &'a App, task: &'a Task, index: usize, len: usize, at: Target, theme: &Theme) -> Element<'a, Msg> {
    let p = theme.extended_palette();
    let open = app.open_task.as_deref() == Some(task.id.as_str());
    let mut info = column![
        row![
            muted(task.key.as_str()).size(style::fs(11)).font(bold()),
            space::horizontal(),
            tip(dot(style::priority_color(theme, task.priority), 8.0), priority_tip(task.priority)),
        ]
        .align_y(Alignment::Center),
        text(task.title.as_str()).size(style::fs(13)),
    ]
    .spacing(4)
    .width(Fill);
    let mut meta = row![].spacing(8).align_y(Alignment::Center);
    if let Some(worker) = worker_label(task, &app.sessions) {
        meta = meta.push(muted(worker).size(style::fs(11)));
    }
    if let Some(round) = round_label(task) {
        meta = meta.push(muted(round).size(style::fs(11)));
    }
    info = info.push(meta);
    if let Some(w) = &task.waiting {
        let label = match w.kind {
            WaitingKind::UsageLimit => format!("Waiting: {}", w.reason),
            _ => "Waiting for you".to_string(),
        };
        info = info.push(
            container(text(label).size(style::fs(11)).font(bold())).padding([1, 7]).style(style::badge(p.warning.base.color)),
        );
    }
    let arrow = |label: &'static str, to: Option<TaskStatus>| {
        button(text(label).size(style::fs(12)))
            .on_press_maybe(to.map(|to| Msg::MoveTask { id: task.id.clone(), to, position: None }))
            .style(style::ghost)
            .padding([1, 8])
    };
    let target_choice = if index == 0 { ReorderTarget::Bottom } else { ReorderTarget::Top };
    let reorder_btn = button(icons::icon(target_choice.icon(), style::fs(12)))
        .on_press_maybe(reorder_msg(&task.id, task.status, index, len))
        .style(style::ghost)
        .padding([1, 6]);
    let id = task.id.clone();
    let move_to = pick_list(&MOVE_TO[task.status.index()][..], None::<Column>, move |c: Column| Msg::MoveTask { id: id.clone(), to: c.0, position: None })
        .placeholder("Move to…")
        .text_size(style::fs(11))
        .padding([2, 6]);
    let controls = row![
        tip(arrow("←", task.status.prev()), "Move left"),
        tip(arrow("→", task.status.next()), "Move right"),
        tip(reorder_btn, target_choice.tooltip()),
        space::horizontal(),
        move_to,
    ]
    .spacing(4)
    .align_y(Alignment::Center);
    let dragged = app.drag.as_ref().is_some_and(|d| d.id == task.id);
    let card = container(column![info, controls].spacing(6))
        .padding([8, 10])
        .width(Fill)
        .style(style::task_card(open || dragged, task.waiting.is_some()));
    // A press starts a drag; a release where it started is a click that opens the task. The
    // buttons and the menu take their own presses.
    let mut area = mouse_area(card)
        .on_press(Msg::DragStart(task.id.clone()))
        .on_release(Msg::Drop)
        .interaction(if app.drag.is_some() { mouse::Interaction::Grabbing } else { mouse::Interaction::Pointer });
    if app.drag.is_some() {
        area = area.on_enter(Msg::DragOver(at));
    }
    area.into()
}

fn priority_tip(priority: TaskPriority) -> &'static str {
    match priority {
        TaskPriority::High => "High priority",
        TaskPriority::Normal => "Normal priority",
        TaskPriority::Low => "Low priority",
    }
}

fn section<'a>(label: &'a str) -> Element<'a, Msg> {
    muted(label).size(style::fs(11)).font(bold()).into()
}

fn task_panel<'a>(app: &'a App, task: &'a Task, theme: &Theme) -> Element<'a, Msg> {
    let p = theme.extended_palette();
    let mut col = column![
        row![
            text(task.key.as_str()).size(style::fs(13)).font(bold()),
            muted(format!("· {}", task.status.label())).size(style::fs(12)),
            space::horizontal(),
            button(text("Close").size(style::fs(12))).on_press(Msg::CloseTask).style(style::ghost).padding([2, 8]),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    ]
    .spacing(12)
    // Room on the right for the scrollbar.
    .padding(Padding { top: 14.0, right: 24.0, bottom: 14.0, left: 16.0 });

    // Waiting for you: an open question, or the usage limit until its reset.
    if let Some(waiting) = &task.waiting {
        let title = if waiting.kind == WaitingKind::UsageLimit { "Waiting: usage limit" } else { "Waiting for you" };
        let banner = column![text(title).size(style::fs(12)).font(bold()), text(waiting.reason.as_str()).size(style::fs(12))]
            .spacing(6);
        col = col.push(container(banner).padding([8, 10]).width(Fill).style(style::banner(p.warning.base.color)));
    }

    // The user's fields.
    let dirty = app.edit_title.trim() != task.title || app.edit_description_text != task.description;
    let priority = |label: &'static str, which: TaskPriority| {
        button(text(label).size(style::fs(12)))
            .on_press(Msg::SetPriority(which))
            .style(if task.priority == which { style::primary_button } else { style::pill_button })
            .padding([3, 10])
    };
    col = col.push(
        column![
            section("TITLE"),
            text_input("Title", &app.edit_title)
                .on_input(Msg::EditTitle)
                .on_submit(Msg::SaveTask)
                .size(style::fs(14))
                .padding([6, 8])
                .style(style::input),
            section("DESCRIPTION"),
            text_editor(&app.edit_description)
                .placeholder("What should the team do? (optional)")
                .on_action(Msg::EditDescription)
                .size(style::fs(13))
                .padding([8, 10])
                .min_height(90)
                .max_height(260)
                .style(style::editor),
            row![
                section("PRIORITY"),
                priority("Low", TaskPriority::Low),
                priority("Normal", TaskPriority::Normal),
                priority("High", TaskPriority::High),
                space::horizontal(),
                button(text("Revert").size(style::fs(12)))
                    .on_press_maybe(dirty.then_some(Msg::RevertTask))
                    .style(style::ghost)
                    .padding([4, 8]),
                button(text("Save").size(style::fs(12)))
                    .on_press_maybe(dirty.then_some(Msg::SaveTask))
                    .style(style::primary_button)
                    .padding([4, 12]),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        ]
        .spacing(6),
    );

    // What the team wrote.
    let mut md_style = markdown::Style::from_palette(theme.palette());
    md_style.font = style::ui_font();
    let settings = markdown::Settings::with_text_size(style::fs(13), md_style);
    col = col.push(rule::horizontal(1).style(rule::weak));
    col = col.push(section("DETAILS"));
    col = col.push(match task.details.as_deref().filter(|d| !d.trim().is_empty()) {
        Some(_) => Element::from(markdown::view(&app.task_details, settings).map(Msg::Link)),
        None => muted("The Coordinator writes the plan here when it picks the task up.").size(style::fs(12)).into(),
    });
    let mut facts = column![].spacing(2);
    if let Some(round) = round_label(task) {
        facts = facts.push(muted(round).size(style::fs(12)));
    }
    if let Some(worker) = worker_label(task, &app.sessions) {
        facts = facts.push(muted(format!("{worker} is on it")).size(style::fs(12)));
    }
    if let Some(worktree) = &task.worktree {
        facts = facts.push(muted(format!("worktree {worktree}")).size(style::fs(12)));
    }
    facts = facts.push(muted(format!("created {}", short_time(task.created_at))).size(style::fs(12)));
    if let Some(done) = task.done_at {
        facts = facts.push(muted(format!("done {}", short_time(done))).size(style::fs(12)));
    }
    col = col.push(facts);

    if let Some(results) = results(app, task, theme) {
        col = col.push(rule::horizontal(1).style(rule::weak)).push(results);
    }
    if let Some(links) = links(app, task) {
        col = col.push(rule::horizontal(1).style(rule::weak)).push(links);
    }

    col = col.push(rule::horizontal(1).style(rule::weak)).push(section("ACTIVITY"));
    match app.task_events.get(&task.id) {
        None => col = col.push(muted("Loading…").size(style::fs(12))),
        Some(events) if events.is_empty() => col = col.push(muted("Nothing yet.").size(style::fs(12))),
        Some(events) => {
            let mut log = column![].spacing(6);
            for e in events {
                log = log.push(
                    column![
                        muted(format!("{} · {}", short_time(e.created_at), actor_label(&e.actor, &app.sessions)))
                            .size(style::fs(11)),
                        text(describe(e)).size(style::fs(12)),
                    ]
                    .spacing(1),
                );
            }
            col = col.push(log);
        }
    }

    // K13: the comment goes to the Coordinator like a chat message.
    let can_send = !app.comment.trim().is_empty();
    col = col.push(
        row![
            text_input("Comment for the team…", &app.comment)
                .on_input(Msg::CommentInput)
                .on_submit(Msg::CommentSend)
                .size(style::fs(13))
                .padding([6, 8])
                .style(style::input),
            button(text("Send").size(style::fs(12)))
                .on_press_maybe(can_send.then_some(Msg::CommentSend))
                .style(style::primary_button)
                .padding([5, 12]),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    );

    col = col.push(rule::horizontal(1).style(rule::weak));
    col = col.push(if app.confirm_delete_task {
        Element::from(
            row![
                muted("Delete this task and its log?").size(style::fs(12)),
                space::horizontal(),
                button(text("Cancel").size(style::fs(12))).on_press(Msg::DeleteTaskCancel).style(style::ghost).padding([4, 8]),
                button(text("Delete").size(style::fs(12))).on_press(Msg::DeleteTaskConfirm).style(style::danger_button).padding([4, 10]),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
    } else {
        row![
            space::horizontal(),
            button(text("Delete task").size(style::fs(12))).on_press(Msg::DeleteTaskAsk).style(style::ghost).padding([4, 8]),
        ]
        .into()
    });

    container(scrollable(col).height(Fill))
        .width(style::fs(14) * 27.0)
        .height(Fill)
        .style(style::task_panel)
        .into()
}

/// The test and review of the current pass; findings grouped critical first.
fn results<'a>(app: &'a App, task: &'a Task, theme: &Theme) -> Option<Element<'a, Msg>> {
    if task.test_result.is_none() && task.review_result.is_none() {
        return None;
    }
    let p = theme.extended_palette();
    let (good, bad) = (p.success.base.color, p.danger.base.color);
    let who = |id: &str| match session_by(&app.sessions, id) {
        Some(s) => s.role.clone().unwrap_or_else(|| s.display_name()),
        None => jgt_core::short_id(id).to_string(),
    };
    let headline = |label: String, color: Color| text(label).size(style::fs(12)).font(bold()).color(color);
    let mut col = column![section("RESULTS")].spacing(6);
    if let Some(test) = &task.test_result {
        let pass = test.status == TestStatus::Pass;
        col = col.push(headline(format!("Test: {}", if pass { "passed" } else { "failed" }), if pass { good } else { bad }));
        col = col.push(muted(format!("{} · {}", who(&test.worker), clock(test.at))).size(style::fs(11)));
        if !test.summary.trim().is_empty() {
            col = col.push(text(test.summary.trim()).size(style::fs(12)));
        }
        for failure in &test.failures {
            col = col.push(text(format!("• {failure}")).size(style::fs(12)));
        }
    }
    if let Some(review) = &task.review_result {
        let approved = review.decision == ReviewDecision::Approved;
        let verdict = if approved { "approved" } else { "changes requested" };
        col = col.push(headline(format!("Review round {}: {verdict}", review.round), if approved { good } else { bad }));
        col = col.push(muted(format!("{} · {}", who(&review.worker), clock(review.at))).size(style::fs(11)));
        let level = |l: Severity| review.findings.iter().filter(|f| f.level() == l).collect::<Vec<&Finding>>();
        for (label, findings) in
            [("Critical", level(Severity::Critical)), ("High", level(Severity::High)), ("Other", level(Severity::Other))]
        {
            if findings.is_empty() {
                continue;
            }
            col = col.push(muted(format!("{label} · {}", findings.len())).size(style::fs(11)).font(bold()));
            for f in findings {
                col = col.push(finding(f));
            }
        }
    }
    Some(col.into())
}

fn finding(f: &Finding) -> Element<'_, Msg> {
    let mut head = String::new();
    if let Some(c) = f.category {
        head.push_str(&format!("[{c}] "));
    }
    if !f.location.is_empty() {
        head.push_str(&f.location);
    }
    let mut col = column![].spacing(1);
    if !head.is_empty() {
        col = col.push(muted(head).size(style::fs(11)));
    }
    col = col.push(text(f.problem.trim()).size(style::fs(12)));
    if !f.required_action.trim().is_empty() {
        col = col.push(muted(format!("→ {}", f.required_action.trim())).size(style::fs(12)));
    }
    container(col).padding([4, 8]).width(Fill).style(style::pill).into()
}

/// The run folder and the chats of the workers that worked on the task.
fn links<'a>(app: &'a App, task: &'a Task) -> Option<Element<'a, Msg>> {
    let mut ids: Vec<&str> = Vec::new();
    ids.extend(task.current_worker.as_deref());
    ids.extend(task.test_result.as_ref().map(|r| r.worker.as_str()));
    ids.extend(task.review_result.as_ref().map(|r| r.worker.as_str()));
    for e in app.task_events.get(&task.id).into_iter().flatten() {
        if let Actor::Worker { session } = &e.actor {
            ids.push(session);
        }
        ids.extend(e.meta.get("worker").and_then(|w| w.as_str()));
    }
    let mut workers: Vec<&Session> = Vec::new();
    for s in ids.into_iter().filter_map(|id| session_by(&app.sessions, id)) {
        if !workers.iter().any(|w| w.id == s.id) {
            workers.push(s);
        }
    }
    if task.run_id.is_none() && workers.is_empty() {
        return None;
    }
    let mut col = column![section("LINKS")].spacing(4);
    if let Some(run) = &task.run_id {
        let folder = format!("{}/.team/runs/{run}", task.workspace);
        col = col.push(tip(
            button(text(format!("Run folder · {run}")).size(style::fs(12)))
                .on_press(Msg::Link(folder))
                .style(style::pill_button)
                .padding([3, 10]),
            "Open the run's briefs, results and report",
        ));
    }
    for w in workers {
        let role = w.role.as_deref().unwrap_or("worker");
        col = col.push(
            button(text(format!("↳ {role} · {}", w.display_name())).size(style::fs(12)).wrapping(text::Wrapping::None))
                .on_press(Msg::Select(w.id.clone()))
                .style(style::session_item(false))
                .width(Fill)
                .padding([3, 8]),
        );
    }
    Some(col.push(Space::new().height(0)).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app::App, hub::Feed};
    use jgt_core::{Event, Harness, SessionStatus};

    fn task(number: i64, status: TaskStatus, position: i64) -> Task {
        Task {
            id: format!("t{number}"),
            workspace: "/w".into(),
            number,
            key: jgt_core::task_key(number),
            title: format!("task {number}"),
            description: String::new(),
            details: None,
            status,
            position,
            priority: TaskPriority::Normal,
            waiting: None,
            review_rounds: 0,
            round_limit: jgt_core::ROUND_LIMIT,
            test_result: None,
            review_result: None,
            run_id: None,
            worktree: None,
            parent_id: None,
            created_at: number,
            updated_at: number,
            done_at: (status == TaskStatus::Done).then_some(1_000 + number),
            archived: false,
            current_worker: None,
        }
    }

    fn session(id: &str, role: Option<&str>) -> Session {
        serde_json::from_value(serde_json::json!({
            "id": id, "harness": Harness::ClaudeCode, "external_id": format!("ext-{id}"),
            "cwd": "/w/", "title": null, "status": SessionStatus::Idle,
            "listen": false, "mirror": false, "archived": false, "unread": 0, "queued": 0,
            "created_at": 0, "updated_at": 0, "last_seen_at": 0, "awaiting_reply": false, "role": role,
        }))
        .expect("a session")
    }

    fn keys<'a>(cards: impl IntoIterator<Item = &'a Task>) -> Vec<String> {
        cards.into_iter().map(|t| t.key.clone()).collect()
    }

    /// The Board column by column, as the app shows it.
    fn order(board: &Board) -> Vec<String> {
        TaskStatus::ALL.iter().map(|s| keys(board.column(*s)).join(",")).collect()
    }

    fn positions(board: &Board, status: TaskStatus) -> Vec<i64> {
        board.column(status).iter().map(|t| t.position).collect()
    }

    #[test]
    fn done_column_shows_20_newest() {
        let board = Board::new((1..=25).map(|n| task(n, TaskStatus::Done, 0)).collect());
        assert_eq!(done_shown(25, false), (20, 5));
        assert_eq!(done_shown(25, true), (25, 0));
        assert_eq!(done_shown(3, false), (3, 0));
        let done = keys(board.done());
        assert_eq!(done.len(), 25);
        assert_eq!(done[0], "JGT-25");
        assert_eq!(done[19], "JGT-6");
    }

    #[test]
    fn cards_sorted_by_position() {
        let tasks = vec![
            task(1, TaskStatus::Backlog, 2),
            task(2, TaskStatus::OnQueue, 0),
            task(3, TaskStatus::Backlog, 0),
            task(4, TaskStatus::Backlog, 1),
        ];
        let board = Board::new(tasks);
        assert_eq!(keys(board.column(TaskStatus::Backlog)), ["JGT-3", "JGT-4", "JGT-1"]);
        assert_eq!(keys(board.column(TaskStatus::OnQueue)), ["JGT-2"]);
        assert!(board.column(TaskStatus::Working).is_empty());
        assert_eq!(board.tasks().iter().map(|t| t.number).collect::<Vec<_>>(), [3, 4, 1, 2]);
    }

    #[test]
    fn move_to_lists_the_other_columns() {
        for status in TaskStatus::ALL.iter().copied() {
            let expected: Vec<Column> = TaskStatus::ALL.iter().copied().filter(|s| *s != status).map(Column).collect();
            assert_eq!(MOVE_TO[status.index()].to_vec(), expected);
        }
    }

    #[test]
    fn upsert_follows_the_hubs_numbering() {
        use TaskStatus::*;
        // Backlog JGT-1..4 at 0..3 (A, B, C, D in the review).
        let mut board = Board::new((1..=4).map(|n| task(n, Backlog, n - 1)).collect());
        // B goes to On Queue: the hub numbers Backlog A0, C1, D2 and sends only B.
        let mut b = task(2, OnQueue, 0);
        b.updated_at = 10;
        assert!(board.upsert(b));
        assert_eq!(order(&board)[..2], ["JGT-1,JGT-3,JGT-4".to_string(), "JGT-2".to_string()]);
        assert_eq!(positions(&board, Backlog), [0, 1, 2]);
        // C's priority set: the hub sends C at position 1 and keeps A, C, D.
        let mut c = task(3, Backlog, 1);
        c.priority = TaskPriority::High;
        c.updated_at = 11;
        assert!(board.upsert(c));
        assert_eq!(order(&board)[0], "JGT-1,JGT-3,JGT-4");
        // A deleted: the hub numbers C0, D1; an edit of D (position 1) keeps it last.
        board.remove("t1");
        assert_eq!(positions(&board, Backlog), [0, 1]);
        let mut d = task(4, Backlog, 1);
        d.updated_at = 12;
        assert!(board.upsert(d));
        assert_eq!(order(&board)[0], "JGT-3,JGT-4");
        // An archived task leaves its column numbered from 0.
        let mut c = task(3, Backlog, 0);
        c.archived = true;
        c.updated_at = 13;
        assert!(board.upsert(c));
        assert_eq!(order(&board)[0], "JGT-4");
        assert_eq!(positions(&board, Backlog), [0]);
    }

    #[test]
    fn older_versions_are_ignored() {
        let (mut app, _) = App::new(None);
        app.boards.insert("/w".into(), Board::new(vec![task(1, TaskStatus::Backlog, 0)]));
        let title = |app: &App| app.boards["/w"].get("t1").unwrap().title.clone();
        let mut newer = task(1, TaskStatus::OnQueue, 0);
        newer.title = "newer".into();
        newer.updated_at = 20;
        let _ = app.update(Msg::Feed(Feed::Event(Box::new(Event::Task { task: newer }))));
        // A late reply to an earlier write, then a move reply, then a Board load, all older.
        let mut older = task(1, TaskStatus::Backlog, 0);
        older.title = "older".into();
        older.updated_at = 10;
        let _ = app.update(Msg::TaskChanged(Ok(older.clone())));
        assert_eq!(title(&app), "newer");
        let moved = jgt_core::api::MoveResponse { task: older.clone(), coordinator: None, queued: false };
        let _ = app.update(Msg::TaskMoved(Ok(Box::new(moved))));
        assert_eq!(title(&app), "newer");
        let _ = app.update(Msg::BoardLoaded { workspace: "/w".into(), result: Ok(vec![older, task(2, TaskStatus::Backlog, 0)]) });
        assert_eq!(title(&app), "newer");
        assert_eq!(order(&app.boards["/w"])[..2], ["JGT-2".to_string(), "JGT-1".to_string()]);
        // The same or a newer version is taken.
        let mut same = task(1, TaskStatus::OnQueue, 0);
        same.title = "same time".into();
        same.updated_at = 20;
        let _ = app.update(Msg::TaskChanged(Ok(same)));
        assert_eq!(title(&app), "same time");
    }

    #[test]
    fn actor_label() {
        let sessions = vec![session("s-tester", Some("tester")), session("s-plain", None)];
        assert_eq!(super::actor_label(&Actor::User, &sessions), "you");
        assert_eq!(super::actor_label(&Actor::Coordinator { session: "c".into() }, &sessions), "Coordinator");
        assert_eq!(super::actor_label(&Actor::Worker { session: "s-tester".into() }, &sessions), "tester");
        assert_eq!(super::actor_label(&Actor::Worker { session: "gone".into() }, &sessions), "worker gone");
        assert_eq!(super::actor_label(&Actor::Hub, &sessions), "hub");

        let mut t = task(1, TaskStatus::Testing, 0);
        t.current_worker = Some("ext-s-tester".into());
        assert_eq!(worker_label(&t, &sessions).as_deref(), Some("↳ tester"));
    }

    #[test]
    fn round_label() {
        let mut t = task(1, TaskStatus::Backlog, 0);
        assert_eq!(super::round_label(&t), None);
        t.status = TaskStatus::Review;
        assert_eq!(super::round_label(&t).as_deref(), Some("round 1/2"));
        // Sent back once: the second cycle, as `task list` shows it.
        t.status = TaskStatus::Working;
        t.review_rounds = 1;
        assert_eq!(super::round_label(&t).as_deref(), Some("round 2/2"));
        t.status = TaskStatus::Backlog;
        t.review_rounds = 2;
        t.round_limit = 3;
        assert_eq!(super::round_label(&t).as_deref(), Some("round 3/3"));
    }

    #[test]
    fn upsert_task_keeps_order() {
        let (mut app, _) = App::new(None);
        let mut coordinator = session("c", Some(jgt_core::COORDINATOR));
        coordinator.cwd = Some("/w/".into());
        assert_eq!(workspace(&coordinator).as_deref(), Some("/w"));
        let board = vec![task(1, TaskStatus::Backlog, 0), task(2, TaskStatus::Backlog, 1), task(3, TaskStatus::Backlog, 2)];
        app.boards.insert("/w".into(), Board::new(board));
        let send = |app: &mut App, event: Event| {
            let _ = app.update(Msg::Feed(Feed::Event(Box::new(event))));
        };
        let order = |app: &App| order(&app.boards["/w"]);

        // JGT-3 moved to the top of Backlog: the hub only sends JGT-3, its neighbours shift down.
        send(&mut app, Event::Task { task: task(3, TaskStatus::Backlog, 0) });
        assert_eq!(order(&app)[0], "JGT-3,JGT-1,JGT-2");
        // JGT-1 goes to On Queue; an edit of JGT-2 keeps its place.
        send(&mut app, Event::Task { task: task(1, TaskStatus::OnQueue, 0) });
        let mut edited = task(2, TaskStatus::Backlog, 1);
        edited.title = "renamed".into();
        send(&mut app, Event::Task { task: edited });
        assert_eq!(order(&app)[..2], ["JGT-3,JGT-2".to_string(), "JGT-1".to_string()]);
        assert_eq!(app.boards["/w"].get("t2").unwrap().title, "renamed");
        // A new task, then a removal and an archive.
        send(&mut app, Event::Task { task: task(4, TaskStatus::Backlog, 2) });
        assert_eq!(order(&app)[0], "JGT-3,JGT-2,JGT-4");
        send(&mut app, Event::TaskRemoved { id: "t3".into(), workspace: "/w".into() });
        let mut archived = task(4, TaskStatus::Backlog, 1);
        archived.archived = true;
        send(&mut app, Event::Task { task: archived });
        assert_eq!(order(&app)[0], "JGT-2");
        // Boards not opened are left alone.
        let mut other = task(9, TaskStatus::Backlog, 0);
        other.workspace = "/elsewhere".into();
        send(&mut app, Event::Task { task: other });
        assert!(!app.boards.contains_key("/elsewhere"));
    }

    #[test]
    fn drops_land_where_the_line_shows() {
        use TaskStatus::*;
        let board = Board::new(vec![task(1, Backlog, 0), task(2, Backlog, 1), task(3, Backlog, 2), task(4, OnQueue, 0), task(5, Done, 0)]);
        // Inside a column: the index among the other cards.
        assert_eq!(drop_move(&board, "t1", Target::Before(Backlog, 3)), Some((Backlog, Some(2))));
        assert_eq!(drop_move(&board, "t1", Target::End(Backlog)), Some((Backlog, Some(2))));
        assert_eq!(drop_move(&board, "t3", Target::Before(Backlog, 0)), Some((Backlog, Some(0))));
        assert_eq!(drop_move(&board, "t1", Target::Before(Backlog, 2)), Some((Backlog, Some(1))));
        // Where it already is: a click.
        assert_eq!(drop_move(&board, "t2", Target::Before(Backlog, 1)), None);
        assert_eq!(drop_move(&board, "t2", Target::Before(Backlog, 2)), None);
        assert_eq!(drop_move(&board, "t3", Target::End(Backlog)), None);
        // Into another column.
        assert_eq!(drop_move(&board, "t2", Target::Before(OnQueue, 0)), Some((OnQueue, Some(0))));
        assert_eq!(drop_move(&board, "t2", Target::End(Working)), Some((Working, None)));
        // Done is ordered by date.
        assert_eq!(drop_move(&board, "t4", Target::Before(Done, 0)), Some((Done, None)));
        assert_eq!(drop_move(&board, "t5", Target::End(Done)), None);
    }

    #[test]
    fn a_drag_ends_on_every_path() {
        let (mut app, _) = App::new(None);
        app.boards.insert("/w".into(), Board::new(vec![task(1, TaskStatus::Backlog, 0), task(2, TaskStatus::Backlog, 1)]));
        // Press and release on the same card: it opens.
        let _ = app.update(Msg::DragStart("t1".into()));
        let _ = app.update(Msg::Drop);
        assert_eq!((app.drag.as_ref(), app.open_task.as_deref()), (None, Some("t1")));
        // Released outside any target, or Esc: nothing moves.
        let _ = app.update(Msg::DragStart("t2".into()));
        let _ = app.update(Msg::DragOver(Target::Before(TaskStatus::Backlog, 0)));
        assert_eq!(app.drag.as_ref().and_then(|d| d.over), Some(Target::Before(TaskStatus::Backlog, 0)));
        let _ = app.update(Msg::DragEnd);
        assert!(app.drag.is_none());
        let _ = app.update(Msg::DragStart("t2".into()));
        let esc = iced::keyboard::Event::KeyPressed {
            key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
            modified_key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
            physical_key: iced::keyboard::key::Physical::Unidentified(iced::keyboard::key::NativeCode::Unidentified),
            location: iced::keyboard::Location::Standard,
            modifiers: iced::keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        };
        let _ = app.update(Msg::Key(esc));
        assert!(app.drag.is_none());
        assert_eq!(app.open_task.as_deref(), Some("t1"), "Esc ended the drag, not the panel");
        // A drop on another place leaves the drag state cleared (the move goes to the hub).
        let _ = app.update(Msg::DragStart("t2".into()));
        let _ = app.update(Msg::DragOver(Target::Before(TaskStatus::Backlog, 0)));
        let _ = app.update(Msg::Drop);
        assert!(app.drag.is_none());
    }

    #[test]
    fn a_queued_task_with_nobody_to_take_it_offers_the_team() {
        let (mut app, _) = App::new(None);
        app.boards.insert("/w".into(), Board::new(vec![task(1, TaskStatus::Backlog, 0)]));
        let moved = |coordinator, queued| {
            let mut t = task(1, TaskStatus::OnQueue, 0);
            t.updated_at = 5;
            Msg::TaskMoved(Ok(Box::new(jgt_core::api::MoveResponse { task: t, coordinator, queued })))
        };
        let _ = app.update(moved(None, false));
        assert_eq!(app.offer_team, None, "a reorder says nothing");
        let _ = app.update(moved(Some(session("c", Some(jgt_core::COORDINATOR))), true));
        assert_eq!(app.offer_team, None);
        let _ = app.update(moved(None, true));
        assert_eq!(app.offer_team.as_deref(), Some("/w"));
        let _ = app.update(Msg::DismissTeamOffer);
        assert_eq!(app.offer_team, None);
        // A comment nobody reads offers it too.
        let _ = app.update(Msg::Commented(Ok(Some("/w".into()))));
        assert_eq!(app.offer_team.as_deref(), Some("/w"));
    }

    #[test]
    fn reorder_target_and_message() {
        use TaskStatus::*;

        // Single card: nowhere to move within column
        assert_eq!(reorder_target(0, 1), None);
        assert!(reorder_msg("t1", Backlog, 0, 1).is_none());

        // Empty column or out of bounds
        assert_eq!(reorder_target(0, 0), None);
        assert_eq!(reorder_target(3, 3), None);
        assert!(reorder_msg("t1", Backlog, 3, 3).is_none());

        // Top card of multiple: moves to bottom (position None)
        assert_eq!(reorder_target(0, 3), Some(ReorderTarget::Bottom));
        assert_eq!(ReorderTarget::Bottom.position(), None);
        assert_eq!(ReorderTarget::Bottom.icon(), icons::Icon::ArrowDownward);
        assert_eq!(ReorderTarget::Bottom.tooltip(), "Send to bottom");
        match reorder_msg("t1", Backlog, 0, 3) {
            Some(Msg::MoveTask { id, to, position }) => {
                assert_eq!(id, "t1");
                assert_eq!(to, Backlog);
                assert_eq!(position, None);
            }
            other => panic!("unexpected message: {other:?}"),
        }

        // Non-top cards of multiple: move to top (position Some(0))
        assert_eq!(reorder_target(1, 3), Some(ReorderTarget::Top));
        assert_eq!(ReorderTarget::Top.position(), Some(0));
        assert_eq!(ReorderTarget::Top.icon(), icons::Icon::ArrowUpward);
        assert_eq!(ReorderTarget::Top.tooltip(), "Send to top");
        match reorder_msg("t2", Backlog, 1, 3) {
            Some(Msg::MoveTask { id, to, position }) => {
                assert_eq!(id, "t2");
                assert_eq!(to, Backlog);
                assert_eq!(position, Some(0));
            }
            other => panic!("unexpected message: {other:?}"),
        }

        assert_eq!(reorder_target(2, 3), Some(ReorderTarget::Top));
        match reorder_msg("t3", OnQueue, 2, 3) {
            Some(Msg::MoveTask { id, to, position }) => {
                assert_eq!(id, "t3");
                assert_eq!(to, OnQueue);
                assert_eq!(position, Some(0));
            }
            other => panic!("unexpected message: {other:?}"),
        }

        // Done column is ordered by date, manual reorder disabled
        assert!(reorder_msg("t4", Done, 0, 3).is_none());
        assert!(reorder_msg("t5", Done, 1, 3).is_none());
    }
}

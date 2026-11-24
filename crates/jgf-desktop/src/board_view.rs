//! Board view model: the ten columns and their cards, ready for the GUI to draw.

use std::{cmp::Ordering, fmt};

use jgf_core::ProjectId;
use jgf_hub::board::{BoardFilter, Column, OutcomeStatus, Task};

/// Every column, in Board order from intake to delivery.
pub const COLUMN_ORDER: [Column; 10] = [
    Column::Backlog,
    Column::OnQueue,
    Column::Working,
    Column::Testing,
    Column::Review,
    Column::ToCommit,
    Column::ToPush,
    Column::ToOpenPr,
    Column::ToMerge,
    Column::Done,
];

/// The whole Board: one view per column, in Board order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardView {
    /// Columns, always all ten and in Board order.
    pub columns: Vec<ColumnView>,
}

/// One Board column with the cards that belong to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnView {
    /// Column this view shows.
    pub column: Column,
    /// Plain-text column title.
    pub title: &'static str,
    /// Cards in the column, ordered by key number.
    pub cards: Vec<CardView>,
}

/// One card of the Board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardView {
    /// Unique board key, as stored on the task.
    pub key: String,
    /// Plain-text card title, unchanged from the task.
    pub title: String,
    /// Project id in upper case, for the badge.
    pub project_badge: String,
    /// Last failing result, when there is one.
    pub status_note: Option<String>,
}

/// The plain-text title of `column`.
pub fn column_title(column: Column) -> &'static str {
    match column {
        Column::Backlog => "Backlog",
        Column::OnQueue => "On Queue",
        Column::Working => "Working",
        Column::Testing => "Testing",
        Column::Review => "Review",
        Column::ToCommit => "To commit",
        Column::ToPush => "To push",
        Column::ToOpenPr => "To open PR",
        Column::ToMerge => "To merge",
        Column::Done => "Done",
    }
}

/// Builds the Board view from `tasks`, keeping cards that match `filter`.
///
/// All ten columns are always present. Inside a column, cards are ordered by the number at the
/// end of their key, so key 2 comes before key 10.
pub fn build_board(tasks: &[Task], filter: &BoardFilter) -> BoardView {
    let mut columns: Vec<ColumnView> = COLUMN_ORDER
        .into_iter()
        .map(|column| ColumnView {
            column,
            title: column_title(column),
            cards: Vec::new(),
        })
        .collect();

    for task in tasks.iter().filter(|task| matches(task, filter)) {
        let card = CardView {
            key: task.key.clone(),
            title: task.title.clone(),
            project_badge: task.project.as_str().to_uppercase(),
            status_note: status_note(task),
        };
        if let Some(column) = columns.iter_mut().find(|view| view.column == task.status) {
            column.cards.push(card);
        }
    }

    for column in &mut columns {
        column
            .cards
            .sort_by(|left, right| compare_keys(&left.key, &right.key));
    }

    BoardView { columns }
}

/// Sorted unique project ids present in `tasks`, for the project filter dropdown.
pub fn project_choices(tasks: &[Task]) -> Vec<String> {
    let mut projects: Vec<String> = tasks
        .iter()
        .map(|task| task.project.as_str().to_string())
        .collect();
    projects.sort();
    projects.dedup();
    projects
}

/// One entry of the project filter dropdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectChoice {
    /// Selected project id, or `None` for "All projects".
    pub project: Option<String>,
    /// Plain-text label shown in the dropdown.
    pub label: String,
}

impl fmt::Display for ProjectChoice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.label)
    }
}

/// Dropdown entries: "All projects" first, then the projects present in `tasks`.
pub fn filter_choices(tasks: &[Task]) -> Vec<ProjectChoice> {
    let mut choices = vec![ProjectChoice {
        project: None,
        label: "All projects".to_string(),
    }];
    choices.extend(
        project_choices(tasks)
            .into_iter()
            .map(|project| ProjectChoice {
                label: project.clone(),
                project: Some(project),
            }),
    );
    choices
}

/// The Board filter for `project`, with no column constraint.
///
/// A project id that does not parse matches every card, like "All projects".
pub fn filter_for(project: Option<&str>) -> BoardFilter {
    BoardFilter {
        project: project.and_then(|raw| ProjectId::parse(raw).ok()),
        column: None,
    }
}

/// Whether `task` passes `filter`.
fn matches(task: &Task, filter: &BoardFilter) -> bool {
    filter
        .project
        .as_ref()
        .is_none_or(|project| *project == task.project)
        && filter.column.is_none_or(|column| column == task.status)
}

/// A note about the last failing test or review, when the task has one.
fn status_note(task: &Task) -> Option<String> {
    if let Some(outcome) = task
        .test_result
        .as_ref()
        .filter(|outcome| outcome.status == OutcomeStatus::Fail)
    {
        return Some(outcome_note("tests failed", &outcome.summary));
    }
    if let Some(outcome) = task
        .review_result
        .as_ref()
        .filter(|outcome| outcome.status == OutcomeStatus::Fail)
    {
        return Some(outcome_note("review failed", &outcome.summary));
    }
    None
}

/// `label`, with `summary` appended when it is not empty.
fn outcome_note(label: &str, summary: &str) -> String {
    if summary.is_empty() {
        label.to_string()
    } else {
        format!("{label}: {summary}")
    }
}

/// Compares card keys by their trailing number, falling back to the full key.
fn compare_keys(left: &str, right: &str) -> Ordering {
    match (key_number(left), key_number(right)) {
        (Some(left_number), Some(right_number)) => {
            left_number.cmp(&right_number).then_with(|| left.cmp(right))
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left.cmp(right),
    }
}

/// The number after the last `-` of `key`, when it parses.
fn key_number(key: &str) -> Option<u64> {
    key.rsplit_once('-')?.1.parse().ok()
}

#[cfg(test)]
mod tests {
    use jgf_core::ProjectId;
    use jgf_hub::board::{Outcome, Task};

    use super::*;

    fn project(raw: &str) -> ProjectId {
        ProjectId::parse(raw).expect("valid project id")
    }

    fn task(key: &str, project_id: &str, status: Column) -> Task {
        Task {
            key: key.to_string(),
            project: project(project_id),
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

    #[test]
    fn empty_board_has_all_ten_columns_in_order() {
        let view = build_board(&[], &BoardFilter::all());
        let columns: Vec<Column> = view.columns.iter().map(|column| column.column).collect();
        assert_eq!(columns, COLUMN_ORDER);
        assert!(view.columns.iter().all(|column| column.cards.is_empty()));
        assert_eq!(
            view.columns
                .iter()
                .map(|column| column.title)
                .collect::<Vec<_>>(),
            [
                "Backlog",
                "On Queue",
                "Working",
                "Testing",
                "Review",
                "To commit",
                "To push",
                "To open PR",
                "To merge",
                "Done",
            ]
        );
    }

    #[test]
    fn cards_sort_by_key_number_inside_a_column() {
        let tasks = [
            task("alpha-10", "alpha", Column::Working),
            task("alpha-2", "alpha", Column::Working),
            task("beta-1", "beta", Column::Working),
        ];
        let view = build_board(&tasks, &BoardFilter::all());
        let working = view
            .columns
            .iter()
            .find(|column| column.column == Column::Working)
            .expect("working column");
        let keys: Vec<&str> = working.cards.iter().map(|card| card.key.as_str()).collect();
        assert_eq!(keys, ["beta-1", "alpha-2", "alpha-10"]);
    }

    #[test]
    fn project_filter_and_badge_are_case_insensitive_to_display() {
        let tasks = [
            task("alpha-1", "alpha", Column::Backlog),
            task("beta-1", "beta", Column::Backlog),
        ];
        let filter = BoardFilter {
            project: Some(project("alpha")),
            column: None,
        };
        let view = build_board(&tasks, &filter);
        let backlog = view
            .columns
            .iter()
            .find(|column| column.column == Column::Backlog)
            .expect("backlog column");
        assert_eq!(backlog.cards.len(), 1);
        assert_eq!(backlog.cards[0].key, "alpha-1");
        assert_eq!(backlog.cards[0].project_badge, "ALPHA");
    }

    #[test]
    fn odd_titles_are_kept_as_plain_text() {
        let cases = [
            ("<b>bold</b>", "<b>bold</b>"),
            ("100% & done", "100% & done"),
            ("  padded  ", "  padded  "),
            ("", ""),
        ];
        for (title, expected) in cases {
            let mut card = task("alpha-1", "alpha", Column::Backlog);
            card.title = title.to_string();
            let view = build_board(&[card], &BoardFilter::all());
            assert_eq!(view.columns[0].cards[0].title, expected);
        }
    }

    #[test]
    fn failed_results_become_status_notes() {
        let cases = [
            (
                Some(Outcome::fail("boom")),
                None,
                Some("tests failed: boom"),
            ),
            (
                Some(Outcome::pass("ok")),
                Some(Outcome::fail("nit")),
                Some("review failed: nit"),
            ),
            (Some(Outcome::pass("ok")), Some(Outcome::pass("fine")), None),
        ];
        for (test_result, review_result, expected) in cases {
            let mut card = task("alpha-1", "alpha", Column::Review);
            card.test_result = test_result;
            card.review_result = review_result;
            let view = build_board(&[card], &BoardFilter::all());
            let note = view.columns[4].cards[0].status_note.as_deref();
            assert_eq!(note, expected);
        }
    }

    #[test]
    fn project_choices_are_sorted_and_unique() {
        let tasks = [
            task("beta-1", "beta", Column::Backlog),
            task("alpha-1", "alpha", Column::Done),
            task("alpha-2", "alpha", Column::Working),
        ];
        assert_eq!(project_choices(&tasks), ["alpha", "beta"]);
        assert!(project_choices(&[]).is_empty());
    }

    #[test]
    fn filter_choices_start_with_all_projects() {
        let tasks = [
            task("beta-1", "beta", Column::Backlog),
            task("alpha-1", "alpha", Column::Done),
        ];
        let choices = filter_choices(&tasks);
        let labels: Vec<&str> = choices.iter().map(|choice| choice.label.as_str()).collect();
        assert_eq!(labels, ["All projects", "alpha", "beta"]);
        assert_eq!(choices[0].project, None);
        assert_eq!(choices[1].project.as_deref(), Some("alpha"));

        let empty = filter_choices(&[]);
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].label, "All projects");
        assert_eq!(empty[0].project, None);
    }

    #[test]
    fn a_hostile_title_stays_a_single_plain_value_and_keeps_the_card_count() {
        let hostile = format!(
            "{}<script>alert('x')</script>\n## not a heading\n- not a list",
            "very long title ".repeat(200)
        );
        let mut card = task("alpha-1", "alpha", Column::Backlog);
        card.title = hostile.clone();

        let view = build_board(&[card], &BoardFilter::all());

        assert_eq!(view.columns[0].cards.len(), 1);
        assert_eq!(view.columns[0].cards[0].title, hostile);
        assert_eq!(
            view.columns
                .iter()
                .map(|column| column.cards.len())
                .sum::<usize>(),
            1
        );
    }

    #[test]
    fn column_card_counts_match_the_tasks_in_each_column() {
        let mut tasks = Vec::new();
        for (index, column) in COLUMN_ORDER.into_iter().enumerate() {
            for number in 0..=index {
                tasks.push(task(&format!("alpha-{index}-{number}"), "alpha", column));
            }
        }

        let view = build_board(&tasks, &BoardFilter::all());
        for column in &view.columns {
            let expected = tasks
                .iter()
                .filter(|task| task.status == column.column)
                .count();
            assert_eq!(column.cards.len(), expected, "{}", column.title);
        }
        assert_eq!(
            view.columns
                .iter()
                .map(|column| column.cards.len())
                .sum::<usize>(),
            tasks.len()
        );
    }
}

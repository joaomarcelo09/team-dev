//! The Board as Markdown, for `jg-team-dev task export` and the app's Export button (K16).

use crate::{Task, TaskPriority, TaskStatus};
use std::path::{Path, PathBuf};

/// Where an export goes unless told otherwise, relative to the workspace (decision 1: outside
/// `.team/`, which git ignores).
pub const EXPORT_PATH: &str = "docs/board.md";

/// The default export file of a workspace.
pub fn export_path(workspace: &str) -> PathBuf {
    Path::new(workspace).join(EXPORT_PATH)
}

/// One section per column, in column order: `- [JGT-12] title (high, round 2/2, waiting: …)`.
/// Columns keep the Board's order; Done lists the newest first.
pub fn to_markdown(tasks: &[Task]) -> String {
    let mut out = String::from("# Board\n");
    for status in TaskStatus::ALL.iter().copied() {
        let mut column: Vec<&Task> = tasks.iter().filter(|t| t.status == status).collect();
        if status == TaskStatus::Done {
            column.sort_by_key(|t| (std::cmp::Reverse(t.done_at.unwrap_or(t.updated_at)), std::cmp::Reverse(t.number)));
        } else {
            column.sort_by_key(|t| (t.position, t.number));
        }
        out.push_str(&format!("\n## {} ({})\n\n", status.label(), column.len()));
        if column.is_empty() {
            out.push_str("_No tasks._\n");
        }
        for t in column {
            out.push_str(&format!("- [{}] {}", t.key, t.title.trim()));
            let mut notes = Vec::new();
            if t.priority != TaskPriority::Normal {
                notes.push(t.priority.to_string());
            }
            if t.shows_round() {
                let (round, limit) = t.round();
                notes.push(format!("round {round}/{limit}"));
            }
            if let Some(w) = &t.waiting {
                notes.push(format!("waiting: {}", w.reason.trim()));
            }
            if t.archived {
                notes.push("archived".into());
            }
            if !notes.is_empty() {
                out.push_str(&format!(" ({})", notes.join(", ")));
            }
            out.push('\n');
        }
    }
    out
}

/// Writes `text` to `path` whole or not at all (a temporary file renamed over it), creating the
/// folder first.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Waiting, WaitingKind};

    #[test]
    fn board_markdown_lists_every_column() {
        let mut a = Task::new("/w", 1, "first", TaskStatus::Backlog);
        a.position = 1;
        let b = Task::new("/w", 2, "second", TaskStatus::Backlog);
        let mut c = Task::new("/w", 3, "fix navbar", TaskStatus::Review);
        c.priority = TaskPriority::High;
        c.review_rounds = 1;
        c.waiting = Some(Waiting {
            kind: WaitingKind::UsageLimit,
            reason: "usage limit, resets 17:00".into(),
            question_id: None,
            since: 0,
            until: Some(1),
        });
        let mut f = Task::new("/w", 6, "add export", TaskStatus::Working);
        f.review_rounds = 1;
        let mut d = Task::new("/w", 4, "old", TaskStatus::Done);
        d.done_at = Some(1);
        let mut e = Task::new("/w", 5, "new", TaskStatus::Done);
        e.done_at = Some(2);
        let md = to_markdown(&[a, b, c, d, e, f]);
        for column in [
            "## Backlog (2)",
            "## On Queue (0)",
            "## Working (1)",
            "## Testing (0)",
            "## Review (1)",
            "## Done (2)",
        ] {
            assert!(md.contains(column), "{column} in\n{md}");
        }
        assert!(md.contains("- [JGT-2] second\n- [JGT-1] first\n"), "{md}");
        assert!(md.contains("- [JGT-3] fix navbar (high, round 2/2, waiting: usage limit, resets 17:00)\n"), "{md}");
        assert!(md.contains("## Working (1)\n\n- [JGT-6] add export (round 2/2)\n"), "{md}");
        assert!(md.find("## Review").unwrap() < md.find("## Done").unwrap());
        assert!(md.contains("- [JGT-5] new\n- [JGT-4] old\n"), "{md}");
        assert!(md.contains("## On Queue (0)\n\n_No tasks._\n"));
    }

    #[test]
    fn exports_are_written_whole() {
        let dir = std::env::temp_dir().join(format!("jgt-export-{}", std::process::id()));
        let path = dir.join("docs").join("board.md");
        write_atomic(&path, "one").unwrap();
        write_atomic(&path, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        assert!(!dir.join("docs").join("board.md.tmp").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

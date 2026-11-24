//! Chat view model: the user's conversation with the Lab Coordinator.
//!
//! The user talks only to the Lab Coordinator: one mailbox holds the requests the user sent
//! and another holds the reports the Lab Coordinator sent back. This module merges the two
//! directions into display lines and validates what the user is about to send. It never
//! changes stored text: the body of a long message is truncated only in the view.

use jgf_hub::{MAX_BODY_BYTES, Message, MessageKind};
use thiserror::Error;

/// Longest body shown per line in the view, in characters.
pub const MAX_DISPLAY_CHARS: usize = 2_000;

/// Longest subject sent with a request, in characters.
pub const MAX_SUBJECT_CHARS: usize = 80;

/// Marker appended to a body that was truncated for display.
const ELLIPSIS: &str = "…";

/// One message of the conversation, ready to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatLine {
    /// Message id, as stored by the router.
    pub id: u64,
    /// Wall-clock time the router stored the message, in milliseconds.
    pub at_ms: u64,
    /// Whether the user sent this line.
    pub from_user: bool,
    /// Short subject, unchanged from the message.
    pub subject: String,
    /// Body, truncated for display.
    pub body: String,
}

/// The whole conversation plus the number of coordinator messages the user has not read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatView {
    /// Lines in display order.
    pub lines: Vec<ChatLine>,
    /// Coordinator messages the user has not read yet.
    pub unread_from_coordinator: usize,
}

/// Merges both directions into one conversation, ordered by `at_ms` and then id.
///
/// Only [`MessageKind::UserRequest`] from the user and [`MessageKind::Report`] from the Lab
/// Coordinator are kept; every other kind is left out.
pub fn build_chat(user_to_lab: &[Message], lab_to_user: &[Message], unread: usize) -> ChatView {
    let mut lines: Vec<ChatLine> = Vec::new();

    for message in user_to_lab
        .iter()
        .filter(|message| message.kind == MessageKind::UserRequest)
    {
        lines.push(chat_line(message, true));
    }
    for message in lab_to_user
        .iter()
        .filter(|message| message.kind == MessageKind::Report)
    {
        lines.push(chat_line(message, false));
    }

    lines.sort_by_key(|line| (line.at_ms, line.id));

    ChatView {
        lines,
        unread_from_coordinator: unread,
    }
}

/// Trims a draft and rejects an empty, oversized or control-laden one.
///
/// A draft may contain newlines and tabs; every other control character, including NUL, is
/// refused.
pub fn validate_draft(text: &str) -> Result<String, ChatError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(ChatError::Empty);
    }
    if trimmed.len() > MAX_BODY_BYTES {
        return Err(ChatError::TooLarge {
            len: trimmed.len(),
            max: MAX_BODY_BYTES,
        });
    }
    if trimmed.chars().any(is_forbidden_control) {
        return Err(ChatError::ControlCharacter);
    }
    Ok(trimmed.to_string())
}

/// The short subject sent with a request: the first line, capped at [`MAX_SUBJECT_CHARS`].
pub fn subject_from(body: &str) -> String {
    let first_line = body.lines().next().unwrap_or_default().trim();
    first_line.chars().take(MAX_SUBJECT_CHARS).collect()
}

/// Whether `character` may not appear in a draft.
fn is_forbidden_control(character: char) -> bool {
    character.is_control() && character != '\n' && character != '\t'
}

/// One stored message as a display line.
fn chat_line(message: &Message, from_user: bool) -> ChatLine {
    ChatLine {
        id: message.id,
        at_ms: message.at_ms,
        from_user,
        subject: message.subject.clone(),
        body: display_body(&message.body),
    }
}

/// Truncates a body to [`MAX_DISPLAY_CHARS`] and marks the cut with an ellipsis.
fn display_body(body: &str) -> String {
    let mut characters = body.chars();
    let head: String = characters.by_ref().take(MAX_DISPLAY_CHARS).collect();
    if characters.next().is_some() {
        format!("{head}{ELLIPSIS}")
    } else {
        head
    }
}

/// Errors raised by [`validate_draft`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ChatError {
    /// The draft is empty or only whitespace.
    #[error("message must not be empty")]
    Empty,

    /// The draft is larger than [`MAX_BODY_BYTES`].
    #[error("message is {len} bytes; the limit is {max}")]
    TooLarge {
        /// Draft size in bytes.
        len: usize,
        /// Configured limit.
        max: usize,
    },

    /// The draft holds a control character other than newline or tab.
    #[error("message contains a control character")]
    ControlCharacter,
}

#[cfg(test)]
mod tests {
    use jgf_hub::{MessageKind, Role};

    use super::*;

    /// A stored message with the given direction and kind.
    fn message(id: u64, at_ms: u64, from_user: bool, kind: MessageKind, body: &str) -> Message {
        Message {
            id,
            from: if from_user {
                Role::User
            } else {
                Role::LabCoordinator
            },
            to: if from_user {
                Role::LabCoordinator
            } else {
                Role::User
            },
            kind,
            project: None,
            subject: format!("subject {id}"),
            body: body.to_string(),
            at_ms,
            wakes: kind.wakes(),
        }
    }

    #[test]
    fn both_directions_merge_in_time_order() {
        let user_to_lab = [
            message(2, 300, true, MessageKind::UserRequest, "second"),
            message(4, 100, true, MessageKind::UserRequest, "first"),
        ];
        let lab_to_user = [
            message(1, 200, false, MessageKind::Report, "reply"),
            message(3, 300, false, MessageKind::Report, "later reply"),
        ];

        let chat = build_chat(&user_to_lab, &lab_to_user, 2);

        assert_eq!(
            chat.lines
                .iter()
                .map(|line| (line.id, line.from_user))
                .collect::<Vec<_>>(),
            [(4, true), (1, false), (2, true), (3, false)]
        );
        assert_eq!(chat.unread_from_coordinator, 2);
    }

    #[test]
    fn ties_break_by_id() {
        let user_to_lab = [message(5, 100, true, MessageKind::UserRequest, "five")];
        let lab_to_user = [message(2, 100, false, MessageKind::Report, "two")];

        let chat = build_chat(&user_to_lab, &lab_to_user, 0);

        assert_eq!(
            chat.lines.iter().map(|line| line.id).collect::<Vec<_>>(),
            [2, 5]
        );
    }

    #[test]
    fn other_kinds_are_left_out() {
        let user_to_lab = [
            message(1, 100, true, MessageKind::UserRequest, "kept"),
            message(2, 200, true, MessageKind::Report, "wrong direction"),
        ];
        let lab_to_user = [
            message(3, 300, false, MessageKind::Report, "kept too"),
            message(4, 400, false, MessageKind::Milestone, "not a report"),
        ];

        let chat = build_chat(&user_to_lab, &lab_to_user, 0);

        assert_eq!(
            chat.lines.iter().map(|line| line.id).collect::<Vec<_>>(),
            [1, 3]
        );
    }

    #[test]
    fn long_bodies_are_truncated_only_in_the_view() {
        let long = "x".repeat(MAX_DISPLAY_CHARS + 5);
        let user_to_lab = [message(1, 100, true, MessageKind::UserRequest, &long)];
        let lab_to_user = [message(2, 200, false, MessageKind::Report, "short")];

        let chat = build_chat(&user_to_lab, &lab_to_user, 0);

        assert_eq!(chat.lines[0].body.chars().count(), MAX_DISPLAY_CHARS + 1);
        assert!(chat.lines[0].body.ends_with(ELLIPSIS));
        assert_eq!(user_to_lab[0].body, long);
        assert_eq!(chat.lines[1].body, "short");
    }

    #[test]
    fn truncation_never_splits_a_character() {
        let long = "é".repeat(MAX_DISPLAY_CHARS + 1);
        let user_to_lab = [message(1, 100, true, MessageKind::UserRequest, &long)];

        let chat = build_chat(&user_to_lab, &[], 0);

        let body = &chat.lines[0].body;
        assert_eq!(body.chars().count(), MAX_DISPLAY_CHARS + 1);
        assert!(body.ends_with(ELLIPSIS));
    }

    #[test]
    fn validate_draft_trims_and_accepts_newlines_and_tabs() {
        assert_eq!(
            validate_draft("  hello\n\tworld  ").expect("valid draft"),
            "hello\n\tworld"
        );
    }

    #[test]
    fn validate_draft_rejects_empty_and_whitespace() {
        for text in ["", "   ", "\n", "\t \n"] {
            assert_eq!(validate_draft(text), Err(ChatError::Empty), "{text:?}");
        }
    }

    #[test]
    fn validate_draft_rejects_forbidden_control_characters() {
        for text in ["a\0b", "a\rb", "a\u{1b}b", "a\u{7}b"] {
            assert_eq!(
                validate_draft(text),
                Err(ChatError::ControlCharacter),
                "{text:?}"
            );
        }
    }

    #[test]
    fn validate_draft_accepts_the_cap_and_rejects_more() {
        let at_cap = "x".repeat(MAX_BODY_BYTES);
        assert_eq!(validate_draft(&at_cap).expect("at the cap"), at_cap);

        let over = "x".repeat(MAX_BODY_BYTES + 1);
        assert_eq!(
            validate_draft(&over),
            Err(ChatError::TooLarge {
                len: MAX_BODY_BYTES + 1,
                max: MAX_BODY_BYTES,
            })
        );
    }

    #[test]
    fn subject_is_the_first_line_capped_at_eighty_characters() {
        assert_eq!(subject_from("hello world\nsecond line"), "hello world");

        let long = format!("{}ignored", "s".repeat(MAX_SUBJECT_CHARS));
        assert_eq!(subject_from(&long).chars().count(), MAX_SUBJECT_CHARS);
        assert_eq!(subject_from("   \nbody"), "");
    }
}

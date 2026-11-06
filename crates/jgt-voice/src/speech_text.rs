use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::path::Path;

/// Converts a message body and metadata into a concise, spoken text representation.
///
/// Rules:
/// - If `meta["speech"]` is a non-empty string, return it immediately.
/// - Otherwise:
///   - Strip Markdown (headings, lists, formatting).
///   - Drop code blocks, tables, images, and raw URLs.
///   - Drop unspeakable inline code spans, keeping only short plain words/phrases.
///   - Replace file paths outside code with their base file name.
///   - Collapse whitespace.
///   - Apply a generous safety cap (~4000 characters, cut at a sentence boundary).
pub fn speech_text(body: &str, meta: &serde_json::Value) -> String {
    // 1. Check meta["speech"]
    if let Some(speech) = meta.get("speech").and_then(|v| v.as_str()) {
        let trimmed = speech.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    if body.trim().is_empty() {
        return String::new();
    }

    // 2. Strip Markdown with pulldown-cmark
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);

    let parser = Parser::new_ext(body, options);

    let mut text_buf = String::new();
    let mut in_code_block = false;
    let mut in_table = false;
    let mut in_image = false;

    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(..)) => in_code_block = true,
            Event::End(TagEnd::CodeBlock) => in_code_block = false,

            Event::Start(Tag::Table(..)) => in_table = true,
            Event::End(TagEnd::Table) => in_table = false,

            Event::Start(Tag::Image { .. }) => in_image = true,
            Event::End(TagEnd::Image) => in_image = false,

            Event::Start(Tag::Heading { .. })
                if !text_buf.is_empty() && !text_buf.ends_with(' ') =>
            {
                text_buf.push(' ');
            }
            Event::End(TagEnd::Heading(..)) => {
                let trimmed_len = text_buf.trim_end().len();
                text_buf.truncate(trimmed_len);
                if !text_buf.ends_with('.') && !text_buf.ends_with('?') && !text_buf.ends_with('!') {
                    text_buf.push('.');
                }
                text_buf.push(' ');
            }

            Event::End(TagEnd::Item) => {
                let trimmed_len = text_buf.trim_end().len();
                text_buf.truncate(trimmed_len);
                if !text_buf.ends_with('.') && !text_buf.ends_with('?') && !text_buf.ends_with('!') {
                    text_buf.push('.');
                }
                text_buf.push(' ');
            }

            Event::End(TagEnd::Paragraph) => {
                text_buf.push(' ');
            }

            Event::Text(t) if !in_code_block && !in_table && !in_image => {
                let s = t.as_ref();
                if text_buf.ends_with(' ')
                    && (s.starts_with('.')
                        || s.starts_with(',')
                        || s.starts_with('!')
                        || s.starts_with('?')
                        || s.starts_with(';')
                        || s.starts_with(':')
                        || s.starts_with(')'))
                {
                    let trimmed_len = text_buf.trim_end().len();
                    text_buf.truncate(trimmed_len);
                }
                text_buf.push_str(s);
            }

            Event::Code(t)
                if !in_code_block
                    && !in_table
                    && !in_image
                    && is_speakable_inline_code(&t) =>
            {
                if !text_buf.is_empty() && !text_buf.ends_with(' ') && !text_buf.ends_with('(') {
                    text_buf.push(' ');
                }
                text_buf.push_str(&t);
            }

            Event::SoftBreak | Event::HardBreak => {
                text_buf.push(' ');
            }

            _ => {}
        }
    }

    // 3. Process tokens: drop URLs and replace file paths
    let processed = process_tokens(&text_buf);

    // 4. Collapse whitespace
    let collapsed = collapse_whitespace(&processed);
    if collapsed.is_empty() {
        return String::new();
    }

    // 5. Generous safety cap (~4000 characters, cut at a sentence boundary)
    truncate_for_speech(&collapsed)
}

/// Checks whether an inline code span is a short plain speakable word or phrase.
///
/// Rules:
/// - Keep only short plain words/phrases: letters, digits, spaces, `-`, `_`, `.`
/// - At most ~24 characters
/// - Forbidden: `/`, `\`, `::`, `()`, `{}`, `=`, `<>`, `;`, `$`, `--`
/// - Must contain at least one alphanumeric character
fn is_speakable_inline_code(code: &str) -> bool {
    const MAX_INLINE_CODE_LEN: usize = 24;

    let trimmed = code.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_INLINE_CODE_LEN {
        return false;
    }

    if trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains("::")
        || trimmed.contains('(')
        || trimmed.contains(')')
        || trimmed.contains('{')
        || trimmed.contains('}')
        || trimmed.contains('=')
        || trimmed.contains('<')
        || trimmed.contains('>')
        || trimmed.contains(';')
        || trimmed.contains('$')
        || trimmed.contains("--")
    {
        return false;
    }

    let is_allowed = |c: char| c.is_alphanumeric() || c.is_whitespace() || c == '-' || c == '_' || c == '.';
    if !trimmed.chars().all(is_allowed) {
        return false;
    }

    if !trimmed.chars().any(|c| c.is_alphanumeric()) {
        return false;
    }

    true
}

/// Checks if a string looks like a web URL.
fn is_url(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("ftp://")
        || lower.starts_with("www.")
}

/// Checks if a token looks like a file path and extracts its basename if so.
fn try_extract_filename(token: &str) -> Option<String> {
    if !token.contains('/') || is_url(token) {
        return None;
    }

    // Fractions like 1/2
    let parts: Vec<&str> = token.split('/').collect();
    if parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }

    // Idiomatic phrases like and/or, either/or, n/a
    let lower = token.to_ascii_lowercase();
    if matches!(lower.as_str(), "and/or" | "either/or" | "n/a") {
        return None;
    }

    // Must look like a path: starts with /, ./, ../, ~/ or has a file extension or multiple components
    let is_path_like = token.starts_with('/')
        || token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with("~/")
        || parts.len() > 2
        || parts.last().map(|p| p.contains('.')).unwrap_or(false);

    if !is_path_like {
        return None;
    }

    let trimmed_token = token.trim_end_matches('/');
    Path::new(trimmed_token)
        .file_name()
        .and_then(|f| f.to_str())
        .map(|s| s.to_string())
}

/// Drops URLs and replaces file paths with file names.
fn process_tokens(input: &str) -> String {
    let mut out = Vec::new();

    for raw_word in input.split_whitespace() {
        // Strip common leading punctuation
        let leading_punct_len = raw_word
            .chars()
            .take_while(|c| matches!(c, '(' | '[' | '{' | '"' | '\''))
            .map(|c| c.len_utf8())
            .sum::<usize>();
        let (prefix, rest) = raw_word.split_at(leading_punct_len);

        // Strip common trailing punctuation
        let trailing_punct_len = rest
            .chars()
            .rev()
            .take_while(|c| matches!(c, ')' | ']' | '}' | '"' | '\'' | ',' | '.' | ':' | ';' | '!' | '?'))
            .map(|c| c.len_utf8())
            .sum::<usize>();
        let (core, suffix) = rest.split_at(rest.len() - trailing_punct_len);

        if is_url(core) {
            // Drop URL entirely
            continue;
        }

        if let Some(filename) = try_extract_filename(core) {
            out.push(format!("{prefix}{filename}{suffix}"));
        } else {
            out.push(raw_word.to_string());
        }
    }

    out.join(" ")
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Splits text into sentences based on `.`, `!`, or `?` followed by space or end of string.
pub fn split_text_sentences(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();

    let mut i = 0;
    while i < len {
        current.push(chars[i]);

        if chars[i] == '.' || chars[i] == '!' || chars[i] == '?' {
            // Check for repeated punctuation (e.g. ..., ?!)
            while i + 1 < len && matches!(chars[i + 1], '.' | '!' | '?') {
                i += 1;
                current.push(chars[i]);
            }

            // Check if next char is whitespace or end of string
            if i + 1 == len || chars[i + 1].is_whitespace() {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    sentences.push(trimmed);
                }
                current.clear();
                // Skip subsequent whitespace
                while i + 1 < len && chars[i + 1].is_whitespace() {
                    i += 1;
                }
            }
        }
        i += 1;
    }

    let remainder = current.trim().to_string();
    if !remainder.is_empty() {
        sentences.push(remainder);
    }

    sentences
}

/// Applies a generous safety cap (~4000 characters, cut at a sentence boundary)
/// to avoid reading enormous messages forever.
fn truncate_for_speech(text: &str) -> String {
    const SAFETY_CAP_CHARS: usize = 4000;

    if text.chars().count() <= SAFETY_CAP_CHARS {
        return text.to_string();
    }

    let sentences = split_text_sentences(text);
    if sentences.is_empty() {
        return String::new();
    }

    let mut kept = Vec::new();
    let mut total_chars = 0;

    for s in &sentences {
        let s_chars = s.chars().count();
        let added_chars = if kept.is_empty() { s_chars } else { s_chars + 1 };

        if total_chars + added_chars <= SAFETY_CAP_CHARS {
            total_chars += added_chars;
            kept.push(s.as_str());
        } else {
            break;
        }
    }

    if kept.is_empty() {
        // The first sentence alone exceeds the safety cap. Cut near the cap at a word boundary.
        let first = &sentences[0];
        let cut_byte_idx = first
            .char_indices()
            .nth(SAFETY_CAP_CHARS)
            .map(|(idx, _)| idx)
            .unwrap_or(first.len());
        let slice = &first[..cut_byte_idx];
        let last_space = slice.rfind(' ').unwrap_or(cut_byte_idx);
        let mut truncated = first[..last_space].trim_end().to_string();
        if !truncated.ends_with('.') && !truncated.ends_with('!') && !truncated.ends_with('?') {
            truncated.push('.');
        }
        truncated
    } else {
        kept.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_meta_speech_override() {
        let meta = json!({ "speech": "Spoken text override" });
        let res = speech_text("Some long markdown body that would otherwise be spoken", &meta);
        assert_eq!(res, "Spoken text override");

        // Empty speech falls back to body
        let meta_empty = json!({ "speech": "   " });
        let res_fallback = speech_text("Normal body text.", &meta_empty);
        assert_eq!(res_fallback, "Normal body text.");
    }

    #[test]
    fn test_empty_input() {
        let meta = json!({});
        assert_eq!(speech_text("", &meta), "");
        assert_eq!(speech_text("   \n\t  ", &meta), "");
    }

    #[test]
    fn test_headings() {
        let body = "# Main Title\n\n## Subheading\n\nAll tasks completed.";
        let meta = json!({});
        let res = speech_text(body, &meta);
        assert_eq!(res, "Main Title. Subheading. All tasks completed.");
    }

    #[test]
    fn test_lists() {
        let body = "Here is what we did:\n* Added speaker\n* Added tts\n* Ran tests";
        let meta = json!({});
        let res = speech_text(body, &meta);
        assert_eq!(res, "Here is what we did: Added speaker. Added tts. Ran tests.");
    }

    #[test]
    fn test_code_fences_dropped() {
        let body = "The test failed with:\n```rust\nfn main() { panic!(\"boom\"); }\n```\nPlease check.";
        let meta = json!({});
        let res = speech_text(body, &meta);
        assert_eq!(res, "The test failed with: Please check.");
    }

    #[test]
    fn test_indented_code_block_dropped() {
        let body = "Here is the plan:\n\n    let x = 1;\n    let y = 2;\n\nAll clear.";
        let meta = json!({});
        let res = speech_text(body, &meta);
        assert_eq!(res, "Here is the plan: All clear.");
    }

    #[test]
    fn test_inline_code_speakable_kept() {
        let body = "Please run `cargo test` now.";
        let meta = json!({});
        let res = speech_text(body, &meta);
        assert_eq!(res, "Please run cargo test now.");
    }

    #[test]
    fn test_inline_code_unspeakable_dropped() {
        let body = "Failed at `fn main() { panic!(); }` and `std::path::Path`. Also check `x = 42;` and `<Vec>`.";
        let meta = json!({});
        let res = speech_text(body, &meta);
        assert_eq!(res, "Failed at and. Also check and.");
    }

    #[test]
    fn test_inline_code_flags_and_paths() {
        // Flags with -- are dropped, but -p is kept; paths with / are dropped, but filename is kept
        let body = "Run `cargo test -p jgt-voice` but not `cargo test --all-targets`. File `crates/jgt/lib.rs` dropped but `lib.rs` kept.";
        let meta = json!({});
        let res = speech_text(body, &meta);
        assert_eq!(
            res,
            "Run cargo test -p jgt-voice but not. File dropped but lib.rs kept."
        );
    }

    #[test]
    fn test_tables_dropped() {
        let body = "Status table:\n\n| Item | Status |\n|---|---|\n| build | ok |\n\nAll clear.";
        let meta = json!({});
        let res = speech_text(body, &meta);
        assert_eq!(res, "Status table: All clear.");
    }

    #[test]
    fn test_links_keep_text_and_drop_urls() {
        let body = "Read [the guide](https://example.com/guide) and visit https://github.com/test directly.";
        let meta = json!({});
        let res = speech_text(body, &meta);
        assert_eq!(res, "Read the guide and visit directly.");
    }

    #[test]
    fn test_file_paths_replaced_by_filename() {
        let body = "Checked crates/jgt-voice/src/lib.rs and /etc/hosts for errors. Also see ./audio/capture.rs.";
        let meta = json!({});
        let res = speech_text(body, &meta);
        assert_eq!(res, "Checked lib.rs and hosts for errors. Also see capture.rs.");
    }

    #[test]
    fn test_long_multi_paragraph_message_spoken_in_full() {
        let body = "Paragraph one introduces the change in detail with several explanations.\n\n\
                    Paragraph two provides additional context on why this was done.\n\n\
                    Paragraph three outlines the test results and confirms all checks passed.\n\n\
                    Paragraph four gives concluding remarks without any truncation suffix.";
        let meta = json!({});
        let res = speech_text(body, &meta);
        assert_eq!(
            res,
            "Paragraph one introduces the change in detail with several explanations. \
             Paragraph two provides additional context on why this was done. \
             Paragraph three outlines the test results and confirms all checks passed. \
             Paragraph four gives concluding remarks without any truncation suffix."
        );
        assert!(!res.contains("Details are in the chat."));
    }

    #[test]
    fn test_moderate_text_no_truncation_or_suffix() {
        let body = "Sentence one is here. Sentence two is here. Sentence three is here. Sentence four is here. Sentence five is here.";
        let meta = json!({});
        let res = speech_text(body, &meta);
        assert_eq!(
            res,
            "Sentence one is here. Sentence two is here. Sentence three is here. Sentence four is here. Sentence five is here."
        );
        assert!(!res.contains("Details are in the chat."));
    }

    #[test]
    fn test_safety_cap_works_at_sentence_boundary() {
        let sentence = "This is a meaningful sentence with enough words to test safety capping. ";
        let body = sentence.repeat(70); // ~5040 chars
        let meta = json!({});
        let res = speech_text(&body, &meta);

        assert!(res.chars().count() <= 4000);
        assert!(res.chars().count() >= 3800);
        assert!(res.ends_with('.'));
        assert!(!res.contains("Details are in the chat."));
    }

    #[test]
    fn test_safety_cap_single_huge_sentence() {
        let body = "word ".repeat(1000); // 5000 chars without sentence enders
        let meta = json!({});
        let res = speech_text(&body, &meta);

        assert!(res.chars().count() <= 4000);
        assert!(res.ends_with('.'));
        assert!(!res.contains("Details are in the chat."));
    }
}

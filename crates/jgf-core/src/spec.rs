//! Specification intake: parsing markdown specs and linting their acceptance criteria.
//!
//! A spec is split at `##` headings. Required sections are matched case-insensitively, and
//! every acceptance criterion must name a command that can be run.

use std::{
    fmt::Write as _,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use sha2::{Digest, Sha256};

use crate::{error::ModelError, project::Project, registry};

/// Directory holding one subdirectory per project.
const PROJECTS_DIR: &str = "projects";
/// File name of the frozen specification inside a project directory.
const SPEC_FILE: &str = "spec.md";
/// Permission bits of a frozen specification.
const FROZEN_MODE: u32 = 0o444;

/// Canonical names of the required sections.
const REQUIRED_SECTIONS: [&str; 5] = [
    "Goal",
    "Scope",
    "Out of scope",
    "Acceptance criteria",
    "Constraints",
];

/// Canonical name of the section whose items must name a command.
const ACCEPTANCE_CRITERIA: &str = "Acceptance criteria";

/// A specification parsed from markdown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spec {
    /// Sections in document order.
    pub sections: Vec<SpecSection>,
    /// List items of each section, parallel to `sections`.
    section_items: Vec<Vec<ListItem>>,
}

impl Spec {
    /// Parses `markdown` into sections.
    ///
    /// Line endings are normalized first, so CRLF and lone CR behave like LF and line numbers
    /// still match the source. Sections start only at top-level CommonMark `##` headings;
    /// deeper headings and headings inside list items, block quotes or code blocks belong to
    /// the section above. The single-`#` document title and any text before the first section
    /// are ignored.
    pub fn parse(markdown: &str) -> Self {
        let markdown = normalize_line_endings(markdown);
        let mut sections = Vec::new();
        let mut section_items = Vec::new();

        for parsed in parse_sections(&markdown) {
            sections.push(parsed.section);
            section_items.push(parsed.items);
        }

        Self {
            sections,
            section_items,
        }
    }

    /// The section whose heading matches `name` after trimming and ignoring case.
    pub fn section(&self, name: &str) -> Option<&SpecSection> {
        let wanted = normalize(name);
        self.sections
            .iter()
            .find(|section| normalize(&section.heading) == wanted)
    }

    /// The list items of the section at `index`.
    fn items(&self, index: usize) -> &[ListItem] {
        self.section_items
            .get(index)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

/// Parses `markdown` into sections.
///
/// This is the free-function form of [`Spec::parse`].
pub fn parse_spec(markdown: &str) -> Spec {
    Spec::parse(markdown)
}

/// A section and the list items collected while walking it.
struct ParsedSection {
    section: SpecSection,
    items: Vec<ListItem>,
}

/// A section being built during the event walk.
struct SectionDraft {
    heading: String,
    body: String,
    body_line: usize,
    items: Vec<ListItem>,
}

/// The heading of the section being read.
struct HeadingDraft {
    text: String,
    line: usize,
}

/// Walks the event stream once and builds the sections and their list items.
fn parse_sections(markdown: &str) -> Vec<ParsedSection> {
    let starts = line_start_offsets(markdown);
    let mut parsed = Vec::new();
    let mut current: Option<SectionDraft> = None;
    let mut heading: Option<HeadingDraft> = None;
    let mut depth = 0usize;
    let mut open_items: Vec<usize> = Vec::new();

    for (event, range) in Parser::new_ext(markdown, Options::empty()).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let is_section =
                    level == HeadingLevel::H2 && depth == 0 && is_atx_h2(markdown, range.start);
                if is_section {
                    finish_section(&mut current, &mut parsed);
                    heading = Some(HeadingDraft {
                        text: String::new(),
                        line: line_number(&starts, range.start),
                    });
                }
                depth += 1;
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(draft) = heading.take() {
                    current = Some(SectionDraft {
                        heading: draft.text.trim().to_string(),
                        body: String::new(),
                        body_line: draft.line + 1,
                        items: Vec::new(),
                    });
                }
                depth = depth.saturating_sub(1);
            }
            Event::Start(tag) => {
                match tag {
                    Tag::Item => {
                        if let Some(section) = current.as_mut() {
                            section.items.push(ListItem {
                                line: line_number(&starts, range.start),
                                has_command: false,
                            });
                            open_items.push(section.items.len() - 1);
                        }
                    }
                    Tag::CodeBlock(_) => mark_command(&mut current, &open_items),
                    _ => {}
                }
                depth += 1;
            }
            Event::End(tag) => {
                if let TagEnd::Item = tag {
                    open_items.pop();
                }
                depth = depth.saturating_sub(1);
            }
            Event::Text(text) => {
                if let Some(draft) = heading.as_mut() {
                    draft.text.push_str(&text);
                } else if let Some(section) = current.as_mut() {
                    section.body.push_str(&text);
                }
            }
            Event::Code(text) => {
                mark_command(&mut current, &open_items);
                if let Some(draft) = heading.as_mut() {
                    draft.text.push_str(&text);
                } else if let Some(section) = current.as_mut() {
                    section.body.push_str(&text);
                }
            }
            // Raw HTML, including comments, is not section content: a section that holds only
            // a placeholder comment is empty.
            Event::Html(_) | Event::InlineHtml(_) => {}
            Event::SoftBreak | Event::HardBreak => {
                if let Some(section) = current.as_mut() {
                    section.body.push('\n');
                }
            }
            _ => {}
        }
    }

    finish_section(&mut current, &mut parsed);
    parsed
}

/// Moves the section being built, if any, into `parsed`.
fn finish_section(current: &mut Option<SectionDraft>, parsed: &mut Vec<ParsedSection>) {
    if let Some(draft) = current.take() {
        parsed.push(ParsedSection {
            section: SpecSection {
                heading: draft.heading,
                body: draft.body,
                body_line: draft.body_line,
            },
            items: draft.items,
        });
    }
}

/// Marks the innermost open list item as containing a command.
fn mark_command(current: &mut Option<SectionDraft>, open_items: &[usize]) {
    let Some(&item) = open_items.last() else {
        return;
    };
    let Some(section) = current.as_mut() else {
        return;
    };
    if let Some(list_item) = section.items.get_mut(item) {
        list_item.has_command = true;
    }
}

/// Replaces CRLF and lone CR line endings with LF.
///
/// Each line ending becomes exactly one LF, so 1-based line numbers stay unchanged.
fn normalize_line_endings(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();

    while let Some(character) = characters.next() {
        if character == '\r' {
            if characters.peek() == Some(&'\n') {
                characters.next();
            }
            normalized.push('\n');
        } else {
            normalized.push(character);
        }
    }

    normalized
}

/// Whether the heading starting at `offset` uses the `##` ATX form rather than a setext underline.
fn is_atx_h2(markdown: &str, offset: usize) -> bool {
    let Some(line) = markdown.get(offset..).and_then(|rest| rest.lines().next()) else {
        return false;
    };
    let line = line.trim_start();
    let hashes = line.bytes().take_while(|&byte| byte == b'#').count();
    hashes == 2 && matches!(line.as_bytes().get(2), None | Some(b' ' | b'\t'))
}

/// Byte offsets where each line of `text` starts.
fn line_start_offsets(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(text.match_indices('\n').map(|(index, _)| index + 1));
    starts
}

/// The 1-based line number containing byte `offset`.
fn line_number(starts: &[usize], offset: usize) -> usize {
    starts.partition_point(|&start| start <= offset)
}

/// One `##` section of a specification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecSection {
    /// Heading text without the `##` markers.
    pub heading: String,
    /// Body text of the section.
    pub body: String,
    /// 1-based line number where the body starts.
    pub body_line: usize,
}

/// A problem found while linting a specification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintIssue {
    /// 1-based line number, or 0 when the issue concerns the whole document.
    pub line: usize,
    /// Human-readable description of the problem.
    pub message: String,
}

/// Lints `spec` and returns every issue found, in section order.
///
/// A missing or empty required section is reported, and under `Acceptance criteria` each list
/// item must name a command: an inline code span or a code block that belongs to the item.
pub fn lint_spec(spec: &Spec) -> Vec<LintIssue> {
    let mut issues = Vec::new();
    for required in REQUIRED_SECTIONS {
        let wanted = normalize(required);
        let sections: Vec<(usize, &SpecSection)> = spec
            .sections
            .iter()
            .enumerate()
            .filter(|(_, section)| normalize(&section.heading) == wanted)
            .collect();
        if sections.is_empty() {
            issues.push(LintIssue {
                line: 0,
                message: format!("missing required section: {required}"),
            });
            continue;
        }
        for (position, (index, section)) in sections.iter().enumerate() {
            if position > 0 {
                issues.push(LintIssue {
                    line: section.body_line,
                    message: format!("duplicate section: {required}"),
                });
            }
            if section.body.trim().is_empty() {
                issues.push(LintIssue {
                    line: section.body_line,
                    message: format!("required section is empty: {required}"),
                });
                continue;
            }
            if required == ACCEPTANCE_CRITERIA {
                issues.extend(acceptance_issues(section, spec.items(*index)));
            }
        }
    }
    issues
}

/// Freezes `source` as the specification of `project`.
///
/// The source is read, parsed and linted; a spec that fails lint is rejected with its issues.
/// A project whose spec is already frozen cannot be frozen again. The accepted spec is written
/// atomically to `<root>/projects/<id>/spec.md` with mode 0444, then its hash and path are
/// stored in the project and saved with [`registry::save_project`]. If saving the project
/// fails, the project is left unchanged so the freeze can be retried.
pub fn freeze_spec(root: &Path, project: &mut Project, source: &Path) -> Result<(), ModelError> {
    if project.spec_sha256.is_some() {
        return Err(ModelError::SpecAlreadyFrozen(project.spec_path.clone()));
    }

    let text = fs::read_to_string(source).map_err(|error| ModelError::io(source, error))?;
    let issues = lint_spec(&Spec::parse(&text));
    if !issues.is_empty() {
        return Err(ModelError::InvalidSpec {
            path: source.to_path_buf(),
            issues,
        });
    }

    let frozen = frozen_path(root, project);
    let parent = frozen.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| ModelError::io(parent, source))?;
    write_frozen_file(&frozen, &text)?;

    let hash = sha256_hex(text.as_bytes());
    let previous_path = std::mem::replace(&mut project.spec_path, frozen);
    let previous_hash = project.spec_sha256.replace(hash);
    match registry::save_project(root, project) {
        Ok(()) => Ok(()),
        Err(error) => {
            project.spec_path = previous_path;
            project.spec_sha256 = previous_hash;
            Err(error)
        }
    }
}

/// Checks that the frozen file of `project` still matches its stored hash.
pub fn verify_spec(project: &Project) -> Result<(), ModelError> {
    let expected = project
        .spec_sha256
        .as_deref()
        .ok_or_else(|| ModelError::MissingSpecHash(project.spec_path.clone()))?;
    let bytes = fs::read(&project.spec_path)
        .map_err(|source| ModelError::io(&project.spec_path, source))?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        return Err(ModelError::SpecHashMismatch {
            path: project.spec_path.clone(),
            expected: expected.to_string(),
            actual,
        });
    }
    Ok(())
}

/// A list item found under the acceptance criteria.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ListItem {
    /// 1-based source line where the item starts.
    line: usize,
    /// Whether the item contains an inline code span or a code block.
    has_command: bool,
}

fn acceptance_issues(section: &SpecSection, items: &[ListItem]) -> Vec<LintIssue> {
    if items.is_empty() {
        return vec![LintIssue {
            line: section.body_line,
            message: "acceptance criteria has no list items".to_string(),
        }];
    }
    items
        .iter()
        .filter(|item| !item.has_command)
        .map(|item| LintIssue {
            line: item.line,
            message: format!(
                "acceptance criterion at line {} has no runnable command",
                item.line
            ),
        })
        .collect()
}

fn normalize(heading: &str) -> String {
    heading.trim().to_lowercase()
}

/// Writes `contents` to `path` atomically with the frozen permissions.
///
/// The temporary file lives in the same directory, so the rename replaces an existing frozen
/// file even when that file is read-only.
fn write_frozen_file(path: &Path, contents: &str) -> Result<(), ModelError> {
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);

    let written = fs::write(&tmp, contents)
        .and_then(|()| fs::set_permissions(&tmp, fs::Permissions::from_mode(FROZEN_MODE)))
        .and_then(|()| fs::rename(&tmp, path));

    written.map_err(|source| {
        let _ = fs::remove_file(&tmp);
        ModelError::io(path, source)
    })
}

fn frozen_path(root: &Path, project: &Project) -> PathBuf {
    root.join(PROJECTS_DIR)
        .join(project.id.as_str())
        .join(SPEC_FILE)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hex
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU32, Ordering},
    };

    use super::*;
    use crate::{
        project::{EnvProfile, ProjectId, ProjectStage},
        registry::load_project,
    };

    /// A small valid specification used as a fixed hash vector.
    const VALID_SPEC: &str = "\
# Tiny spec

## Goal
Ship a tiny app.

## Scope
One page.

## Out of scope
Accounts.

## Acceptance criteria
- `cargo test` passes.

## Constraints
Only std.
";

    /// SHA-256 of [`VALID_SPEC`].
    const VALID_SPEC_SHA256: &str =
        "5867627e1a811b816dfe431d67225d684fbce3a45a3d6d21ec43a3554dbeec76";

    /// Temporary root removed when the test ends.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("jgf-core-spec-{}-{suffix}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp root");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sample_project(id: &str) -> Project {
        Project {
            id: ProjectId::parse(id).expect("valid id"),
            name: "Example".into(),
            spec_path: PathBuf::from("/unused/spec.md"),
            spec_sha256: None,
            repos: Vec::new(),
            stage: ProjectStage::Mvp,
            env_profile: EnvProfile::default(),
            recipes: Vec::new(),
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    #[test]
    fn valid_spec_lints_clean() {
        let spec = Spec::parse(VALID_SPEC);
        assert_eq!(spec.sections.len(), REQUIRED_SECTIONS.len());
        assert!(
            lint_spec(&spec).is_empty(),
            "unexpected issues: {:?}",
            lint_spec(&spec)
        );
    }

    #[test]
    fn each_missing_section_is_reported() {
        for missing in REQUIRED_SECTIONS {
            let text = VALID_SPEC.replace(&format!("## {missing}"), "## Not required");
            let issues = lint_spec(&Spec::parse(&text));
            assert!(
                issues.iter().any(|issue| issue.message.contains(missing)),
                "no issue for missing {missing}: {issues:?}"
            );
        }
    }

    #[test]
    fn an_empty_required_section_is_reported() {
        let text = VALID_SPEC.replace("Ship a tiny app.", "");
        let issues = lint_spec(&Spec::parse(&text));

        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].message.contains("Goal"));
        assert!(issues[0].message.contains("empty"));
    }

    #[test]
    fn an_html_comment_only_section_is_reported_as_empty() {
        let text = VALID_SPEC.replace("Only std.", "<!-- TODO -->");
        let issues = lint_spec(&Spec::parse(&text));

        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].line, 16);
        assert_eq!(issues[0].message, "required section is empty: Constraints");
    }

    #[test]
    fn a_multiline_html_comment_only_section_is_reported_as_empty() {
        let text = VALID_SPEC.replace("Ship a tiny app.", "<!--\nfill in\n-->");
        let issues = lint_spec(&Spec::parse(&text));

        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].line, 4);
        assert_eq!(issues[0].message, "required section is empty: Goal");
    }

    #[test]
    fn an_inline_html_only_section_is_reported_as_empty() {
        let text = VALID_SPEC.replace("One page.", "<span></span>");
        let issues = lint_spec(&Spec::parse(&text));

        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].line, 7);
        assert_eq!(issues[0].message, "required section is empty: Scope");
    }

    #[test]
    fn a_section_with_a_comment_and_text_is_not_empty() {
        let text = VALID_SPEC.replace("One page.", "<!-- note -->\nOne page.");
        let spec = Spec::parse(&text);

        assert!(lint_spec(&spec).is_empty(), "{:?}", lint_spec(&spec));
    }

    #[test]
    fn acceptance_criteria_without_list_items_is_reported() {
        let text = VALID_SPEC.replace("- `cargo test` passes.", "It works.");
        let issues = lint_spec(&Spec::parse(&text));

        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].message.contains("no list items"));
    }

    #[test]
    fn a_criterion_without_a_command_is_reported_with_its_line() {
        let text = "\
# Example

## Goal
Do it.

## Scope
Thing.

## Out of scope
Nothing.

## Acceptance criteria
- `cargo test` passes.
- The app is fast.
- `cargo fmt` passes.

## Constraints
None.
";
        let issues = lint_spec(&Spec::parse(text));

        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].line, 14);
        assert!(issues[0].message.contains("no runnable command"));
    }

    #[test]
    fn text_after_a_blank_line_is_not_part_of_the_item_above_it() {
        let text = "\
# Example

## Goal
Do it.

## Scope
Thing.

## Out of scope
Nothing.

## Acceptance criteria
- The app is fast.

Run every check with `make`.

## Constraints
None.
";
        let issues = lint_spec(&Spec::parse(text));

        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].line, 13);
        assert!(issues[0].message.contains("no runnable command"));
    }

    #[test]
    fn an_unclosed_fence_does_not_swallow_the_next_item() {
        let text = "\
# Example

## Goal
Do it.

## Scope
Thing.

## Out of scope
Nothing.

## Acceptance criteria
- Run:
  ```
  no closing line
- The app is fast.

## Constraints
None.
";
        let issues = lint_spec(&Spec::parse(text));

        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].line, 16);
        assert!(issues[0].message.contains("no runnable command"));
    }

    #[test]
    fn headings_with_a_closing_hash_sequence_lint_clean() {
        let text = "\
# Example

## Goal ##
Do it.

## Scope ##
Thing.

## Out of scope ##
Nothing.

## Acceptance criteria ##
- `cargo test` passes.

## Constraints ##
None.
";
        let spec = Spec::parse(text);

        assert_eq!(spec.sections.len(), REQUIRED_SECTIONS.len(), "{spec:?}");
        assert_eq!(lint_spec(&spec), Vec::new());
    }

    #[test]
    fn a_longer_fence_shields_inner_fences_and_headings() {
        let text = "\
# Example

## Goal
Do it.

## Scope
Thing.

## Out of scope
Nothing.

## Acceptance criteria
- Run:
  ````
  ```
  ## Notes
  ```
  ````
- The app is fast.

## Constraints
None.
";
        let spec = Spec::parse(text);
        let issues = lint_spec(&spec);

        assert_eq!(spec.sections.len(), REQUIRED_SECTIONS.len(), "{spec:?}");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].line, 19);
        assert!(issues[0].message.contains("no runnable command"));
    }

    #[test]
    fn a_fenced_block_indented_under_an_item_is_a_command() {
        let text = "\
## Goal
Do it.

## Scope
Thing.

## Out of scope
Nothing.

## Acceptance criteria
- Run the checks:
  ```
  cargo test
  ```

## Constraints
None.
";
        assert!(
            lint_spec(&Spec::parse(text)).is_empty(),
            "an indented fenced block must count as a command"
        );
    }

    #[test]
    fn several_issues_are_reported_together() {
        let text = "\
# Example

## Goal

## Out of scope
Nothing.

## Acceptance criteria
- The app is fast.

## Constraints
None.
";
        let issues = lint_spec(&Spec::parse(text));

        assert_eq!(issues.len(), 3, "{issues:?}");
        assert!(issues.iter().any(|issue| issue.message.contains("Goal")));
        assert!(issues.iter().any(|issue| issue.message.contains("Scope")));
        assert!(
            issues
                .iter()
                .any(|issue| issue.message.contains("no runnable command"))
        );
    }

    #[test]
    fn freeze_writes_a_read_only_spec_and_updates_the_project() {
        let root = TempRoot::new();
        let source = root.path().join("source.md");
        fs::write(&source, VALID_SPEC).expect("write source");
        let mut project = sample_project("example");

        freeze_spec(root.path(), &mut project, &source).expect("freeze spec");

        let frozen = root
            .path()
            .join(PROJECTS_DIR)
            .join("example")
            .join(SPEC_FILE);
        assert_eq!(project.spec_path, frozen);
        assert_eq!(project.spec_sha256.as_deref(), Some(VALID_SPEC_SHA256));
        assert_eq!(mode(&frozen), FROZEN_MODE);

        let bytes = fs::read(&frozen).expect("read frozen spec");
        assert_eq!(bytes, VALID_SPEC.as_bytes());
        assert_eq!(sha256_hex(&bytes), VALID_SPEC_SHA256);

        let loaded = load_project(root.path(), &project.id).expect("load project");
        assert_eq!(loaded, project);
    }

    #[test]
    fn a_second_freeze_is_refused() {
        let root = TempRoot::new();
        let source = root.path().join("source.md");
        fs::write(&source, VALID_SPEC).expect("write source");
        let mut project = sample_project("example");
        freeze_spec(root.path(), &mut project, &source).expect("first freeze");
        let frozen = project.spec_path.clone();

        let error = freeze_spec(root.path(), &mut project, &source).expect_err("second freeze");

        assert!(matches!(error, ModelError::SpecAlreadyFrozen(_)));
        assert_eq!(project.spec_sha256.as_deref(), Some(VALID_SPEC_SHA256));
        assert_eq!(
            fs::read_to_string(&frozen).expect("frozen spec"),
            VALID_SPEC
        );
    }

    #[test]
    fn a_freeze_can_be_retried_after_a_failed_save() {
        let root = TempRoot::new();
        let source = root.path().join("source.md");
        fs::write(&source, VALID_SPEC).expect("write source");
        let mut project = sample_project("example");

        let blocked = root
            .path()
            .join(PROJECTS_DIR)
            .join("example")
            .join("project.toml");
        fs::create_dir_all(&blocked).expect("block the project record");

        let error = freeze_spec(root.path(), &mut project, &source).expect_err("save must fail");
        assert!(matches!(error, ModelError::Io { .. }), "{error:?}");
        assert_eq!(project.spec_path, PathBuf::from("/unused/spec.md"));
        assert!(project.spec_sha256.is_none());

        fs::remove_dir(&blocked).expect("unblock the project record");
        freeze_spec(root.path(), &mut project, &source).expect("retry freeze");

        let frozen = root
            .path()
            .join(PROJECTS_DIR)
            .join("example")
            .join(SPEC_FILE);
        assert_eq!(project.spec_path, frozen);
        assert_eq!(project.spec_sha256.as_deref(), Some(VALID_SPEC_SHA256));
        assert_eq!(mode(&frozen), FROZEN_MODE);
        assert_eq!(
            fs::read_to_string(&frozen).expect("frozen spec"),
            VALID_SPEC
        );
        assert_eq!(
            load_project(root.path(), &project.id).expect("load project"),
            project
        );
    }

    #[test]
    fn an_invalid_spec_leaves_nothing_on_disk() {
        let root = TempRoot::new();
        let source = root.path().join("source.md");
        let invalid = VALID_SPEC.replace("- `cargo test` passes.", "- The app is fast.");
        fs::write(&source, invalid).expect("write source");
        let mut project = sample_project("example");

        let error = freeze_spec(root.path(), &mut project, &source).expect_err("invalid spec");

        match error {
            ModelError::InvalidSpec { issues, .. } => assert!(!issues.is_empty()),
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(!root.path().join(PROJECTS_DIR).exists());
        assert!(project.spec_sha256.is_none());
    }

    #[test]
    fn verify_spec_detects_an_edited_frozen_file() {
        let root = TempRoot::new();
        let source = root.path().join("source.md");
        fs::write(&source, VALID_SPEC).expect("write source");
        let mut project = sample_project("example");
        freeze_spec(root.path(), &mut project, &source).expect("freeze spec");

        verify_spec(&project).expect("frozen spec verifies");

        fs::set_permissions(&project.spec_path, fs::Permissions::from_mode(0o644))
            .expect("make frozen spec writable");
        fs::write(
            &project.spec_path,
            VALID_SPEC.replace("Only std.", "All the things."),
        )
        .expect("edit frozen spec");

        let error = verify_spec(&project).expect_err("edited spec must not verify");
        assert!(matches!(error, ModelError::SpecHashMismatch { .. }));
    }

    #[test]
    fn verify_spec_requires_a_hash() {
        let project = sample_project("example");
        assert!(matches!(
            verify_spec(&project),
            Err(ModelError::MissingSpecHash(_))
        ));
    }

    #[test]
    fn a_heading_inside_a_fence_does_not_start_a_section() {
        let text = "\
# Example

## Goal
Do it.

## Scope
Thing.

## Out of scope
Nothing.

## Acceptance criteria
- Run the checks:
  ```
  ## not a heading
  cargo test
  ```
- The app is fast.

## Constraints
None.
";
        let spec = Spec::parse(text);
        let issues = lint_spec(&spec);

        assert_eq!(spec.sections.len(), REQUIRED_SECTIONS.len(), "{spec:?}");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].line, 18);
        assert!(issues[0].message.contains("no runnable command"));
    }

    #[test]
    fn a_fenced_goal_does_not_satisfy_a_missing_goal_section() {
        let text = "\
# Example

## Scope
Thing.

Example:

```
## Goal
Fenced example.
```

## Out of scope
Nothing.

## Acceptance criteria
- `cargo test` passes.

## Constraints
None.
";
        let issues = lint_spec(&Spec::parse(text));

        assert!(
            issues
                .iter()
                .any(|issue| issue.message == "missing required section: Goal"),
            "fenced heading must not satisfy Goal: {issues:?}"
        );
    }

    #[test]
    fn a_fenced_comment_does_not_shift_reported_lines() {
        let text = "\
# Example

## Goal
Do it.

## Scope
Thing.

## Out of scope
Nothing.

## Acceptance criteria
- Run the checks:
  ```
  # comment
  cargo test
  ```
- The app is fast.

## Constraints
None.
";
        let issues = lint_spec(&Spec::parse(text));

        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].line, 18);
        assert!(issues[0].message.contains("no runnable command"));
    }

    #[test]
    fn a_tilde_fence_hides_headings_like_a_backtick_fence() {
        let text = "\
## Goal
Do it.

## Scope
Thing.

## Out of scope
Nothing.

## Acceptance criteria
- Run the checks:
  ~~~
  ## not a heading
  cargo test
  ~~~
- The app is fast.

## Constraints
None.
";
        let spec = Spec::parse(text);
        let issues = lint_spec(&spec);

        assert_eq!(spec.sections.len(), REQUIRED_SECTIONS.len(), "{spec:?}");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].line, 16);
        assert!(issues[0].message.contains("no runnable command"));
    }

    #[test]
    fn a_longer_outer_fence_shields_inner_fences_and_headings() {
        let text = "\
# Example

## Goal
Do it.

## Scope
Thing.

## Out of scope
Nothing.

## Acceptance criteria
- Run:
  ````
  ```
  ## not a heading
  ```
  ````
- The app is fast.

## Constraints
None.
";
        let spec = Spec::parse(text);
        let issues = lint_spec(&spec);

        assert_eq!(spec.sections.len(), REQUIRED_SECTIONS.len(), "{spec:?}");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].line, 19);
        assert!(issues[0].message.contains("no runnable command"));
    }

    #[test]
    fn an_info_string_does_not_close_a_fence() {
        let text = "\
## Goal
Do it.

## Scope
Thing.

## Out of scope
Nothing.

## Acceptance criteria
- Run:
  ```
  ```rust
  ## not a heading
  ```
- The app is fast.

## Constraints
None.
";
        let spec = Spec::parse(text);
        let issues = lint_spec(&spec);

        assert_eq!(spec.sections.len(), REQUIRED_SECTIONS.len(), "{spec:?}");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].line, 16);
        assert!(issues[0].message.contains("no runnable command"));
    }

    #[test]
    fn a_closing_fence_must_be_at_least_as_long_as_the_opening_one() {
        let longer = "\
## Goal
```
````
## Scope
Thing.
";
        let spec = Spec::parse(longer);
        assert_eq!(spec.sections.len(), 2, "{spec:?}");
        assert!(spec.section("Scope").is_some(), "{spec:?}");

        let shorter = "\
## Goal
````
```
## Scope
Thing.
";
        let spec = Spec::parse(shorter);
        assert_eq!(spec.sections.len(), 1, "{spec:?}");
        assert!(
            spec.section("Goal")
                .expect("goal")
                .body
                .contains("## Scope"),
            "a shorter closing fence must not end the block: {spec:?}"
        );
    }

    #[test]
    fn an_unclosed_fence_runs_to_the_end_without_panicking() {
        let text = "\
## Goal
Do it.

## Scope
Thing.

## Out of scope
Nothing.

## Acceptance criteria
- Run the checks:
  ```
  ## Constraints
";
        let spec = Spec::parse(text);
        let issues = lint_spec(&spec);

        assert_eq!(spec.sections.len(), 4, "{spec:?}");
        assert!(
            issues
                .iter()
                .any(|issue| issue.message == "missing required section: Constraints"),
            "an unclosed fence must swallow later headings: {issues:?}"
        );
    }

    #[test]
    fn a_cr_only_spec_reports_the_right_line_without_panicking() {
        let text = VALID_SPEC
            .replace(
                "- `cargo test` passes.",
                "- `cargo test` passes.\n- The app is fast.",
            )
            .replace('\n', "\r");
        let spec = Spec::parse(&text);
        let issues = lint_spec(&spec);

        assert_eq!(spec.sections.len(), REQUIRED_SECTIONS.len(), "{spec:?}");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].line, 14);
        assert!(issues[0].message.contains("no runnable command"));
    }

    #[test]
    fn a_spec_with_mixed_line_endings_reports_the_right_line() {
        let text = concat!(
            "# Example\r\n",
            "\r\n",
            "## Goal\r",
            "Do it.\n",
            "\n",
            "## Scope\r\n",
            "Thing.\r",
            "\r",
            "## Out of scope\n",
            "Nothing.\r\n",
            "\r\n",
            "## Acceptance criteria\r\n",
            "- `cargo test` passes.\r",
            "- The app is fast.\n",
            "\n",
            "## Constraints\r\n",
            "None.\n",
        );
        let spec = Spec::parse(text);
        let issues = lint_spec(&spec);

        assert_eq!(spec.sections.len(), REQUIRED_SECTIONS.len(), "{spec:?}");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].line, 14);
        assert!(issues[0].message.contains("no runnable command"));
    }

    #[test]
    fn a_spec_without_a_trailing_newline_reports_the_right_line() {
        let text = "\
# Example

## Goal
Do it.

## Scope
Thing.

## Out of scope
Nothing.

## Acceptance criteria
- `cargo test` passes.
- The app is fast.

## Constraints
None.";
        let spec = Spec::parse(text);
        let issues = lint_spec(&spec);

        assert_eq!(spec.sections.len(), REQUIRED_SECTIONS.len(), "{spec:?}");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].line, 14);
        assert!(issues[0].message.contains("no runnable command"));
    }

    #[test]
    fn multi_byte_text_keeps_the_reported_line_correct() {
        let text = "\
# Example

## Goal
Do it — even when the app is fast.

## Scope
Thing.

## Out of scope
Nothing.

## Acceptance criteria
- `cargo test` passes.
- 日本語 — the app is fast.

## Constraints
None.
";
        let spec = Spec::parse(text);
        let issues = lint_spec(&spec);

        assert_eq!(spec.sections.len(), REQUIRED_SECTIONS.len(), "{spec:?}");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].line, 14);
        assert!(issues[0].message.contains("no runnable command"));
    }

    #[test]
    fn every_acceptance_criteria_section_is_linted_and_duplicates_reported() {
        let text = "\
# Example

## Goal
Do it.

## Scope
Thing.

## Out of scope
Nothing.

## Acceptance criteria
- `cargo test` passes.

## Acceptance criteria
- The app is fast.

## Constraints
None.
";
        let spec = Spec::parse(text);
        let issues = lint_spec(&spec);

        assert_eq!(spec.sections.len(), REQUIRED_SECTIONS.len() + 1, "{spec:?}");
        assert_eq!(issues.len(), 2, "{issues:?}");
        assert!(
            issues
                .iter()
                .any(|issue| issue.message.contains("duplicate section") && issue.line == 16),
            "{issues:?}"
        );
        assert!(
            issues
                .iter()
                .any(|issue| issue.message.contains("no runnable command") && issue.line == 16),
            "{issues:?}"
        );
    }

    #[test]
    fn an_indented_heading_inside_an_item_does_not_split_the_section() {
        let text = "\
# Example

## Goal
Do it.

## Scope
Thing.

## Out of scope
Nothing.

## Acceptance criteria
- `cargo test` passes.
  ## Notes
- The app is fast.

## Constraints
None.
";
        let spec = Spec::parse(text);
        let issues = lint_spec(&spec);

        assert_eq!(spec.sections.len(), REQUIRED_SECTIONS.len(), "{spec:?}");
        assert!(spec.section("Notes").is_none(), "{spec:?}");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].line, 15);
        assert!(issues[0].message.contains("no runnable command"));
    }

    #[test]
    fn a_heading_inside_a_block_quote_does_not_split_the_section() {
        let text = "\
# Example

## Goal
Do it.

## Scope
Thing.

## Out of scope
Nothing.

## Acceptance criteria
> ## Quoted notes
- The app is fast.

## Constraints
None.
";
        let spec = Spec::parse(text);
        let issues = lint_spec(&spec);

        assert_eq!(spec.sections.len(), REQUIRED_SECTIONS.len(), "{spec:?}");
        assert!(spec.section("Quoted notes").is_none(), "{spec:?}");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].line, 14);
        assert!(issues[0].message.contains("no runnable command"));
    }

    #[test]
    fn empty_and_cr_only_input_report_the_missing_sections() {
        for text in ["", "\r"] {
            let spec = Spec::parse(text);
            let issues = lint_spec(&spec);

            assert!(spec.sections.is_empty(), "{spec:?}");
            assert_eq!(issues.len(), REQUIRED_SECTIONS.len(), "{issues:?}");
            assert!(
                issues
                    .iter()
                    .all(|issue| issue.message.starts_with("missing required section")),
                "{issues:?}"
            );
        }
    }
}

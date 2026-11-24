//! Guard for the E2E toy fixture: its spec must parse and lint clean.

use std::{fs, path::Path};

use jgf_core::spec::{lint_spec, parse_spec};

#[test]
fn toy_spec_parses_and_lints_clean() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../e2e/toy/SPEC.md");
    let markdown = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    let spec = parse_spec(&markdown);

    assert_eq!(
        spec.sections.len(),
        5,
        "toy spec must have the five required sections: {:?}",
        spec.sections
    );
    let issues = lint_spec(&spec);
    assert!(issues.is_empty(), "toy spec has lint issues: {issues:?}");
}

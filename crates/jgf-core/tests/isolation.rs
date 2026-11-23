//! Isolation guard: no stable crate (`jgt-*`) may depend on a Dark Factory crate
//! (`jgf-*` or `jg-sandbox`). Dark Factory may depend on stable crates, never the reverse.

use std::{fs, path::Path};

const FORBIDDEN_PREFIXES: [&str; 2] = ["jgf-", "jg-sandbox"];
const DEPENDENCY_TABLES: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

/// Names of the Dark Factory crates a manifest depends on.
fn forbidden_dependencies(manifest: &str) -> Vec<String> {
    let doc: toml::Table = manifest.parse().expect("valid Cargo.toml");
    DEPENDENCY_TABLES
        .iter()
        .filter_map(|table| doc.get(*table)?.as_table())
        .flat_map(|deps| deps.keys())
        .filter(|name| FORBIDDEN_PREFIXES.iter().any(|p| name.starts_with(p)))
        .cloned()
        .collect()
}

#[test]
fn stable_crates_do_not_depend_on_dark_factory() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut checked = 0;
    for entry in fs::read_dir(&crates).expect("crates dir") {
        let path = entry.expect("entry").path();
        let stable = path.file_name().is_some_and(|n| n.to_string_lossy().starts_with("jgt-"));
        if !stable {
            continue;
        }
        let manifest = fs::read_to_string(path.join("Cargo.toml")).expect("manifest");
        let found = forbidden_dependencies(&manifest);
        assert!(found.is_empty(), "{} depends on Dark Factory crates: {found:?}", path.display());
        checked += 1;
    }
    assert!(checked > 0, "no stable crate found: the guard checked nothing");
}

#[test]
fn guard_detects_a_forbidden_dependency() {
    let manifest = "[package]\nname = \"x\"\n[dependencies]\njgf-core = { path = \"../jgf-core\" }\nserde = \"1\"\n";
    assert_eq!(forbidden_dependencies(manifest), vec!["jgf-core".to_string()]);
    assert!(forbidden_dependencies("[dependencies]\nserde = \"1\"\n").is_empty());
}

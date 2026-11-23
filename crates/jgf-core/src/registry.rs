//! Persistence of labs and projects under a caller-provided root.
//!
//! Callers pass [`crate::paths::data_dir`] as the root; tests pass a temporary directory.

use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use serde::de::DeserializeOwned;

use crate::{
    error::ModelError,
    lab::LabConfig,
    project::{Project, ProjectId},
};

/// File name of the lab configuration inside the root.
const LAB_FILE: &str = "lab.toml";
/// Directory holding one subdirectory per project.
const PROJECTS_DIR: &str = "projects";
/// File name of a project inside its directory.
const PROJECT_FILE: &str = "project.toml";

/// Saves `lab` at `<root>/lab.toml`.
pub fn save_lab(root: &Path, lab: &LabConfig) -> Result<(), ModelError> {
    save(&lab_path(root), lab)
}

/// Loads the lab from `<root>/lab.toml`.
pub fn load_lab(root: &Path) -> Result<LabConfig, ModelError> {
    load(&lab_path(root))
}

/// Saves `project` at `<root>/projects/<id>/project.toml`.
pub fn save_project(root: &Path, project: &Project) -> Result<(), ModelError> {
    save(&project_path(root, &project.id), project)
}

/// Loads the project with `id` from `<root>/projects/<id>/project.toml`.
pub fn load_project(root: &Path, id: &ProjectId) -> Result<Project, ModelError> {
    load(&project_path(root, id))
}

/// Lists the ids of all stored projects, sorted by id.
///
/// A missing projects directory is empty, not an error.
pub fn list_projects(root: &Path) -> Result<Vec<ProjectId>, ModelError> {
    let dir = root.join(PROJECTS_DIR);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(ModelError::io(&dir, source)),
    };

    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ModelError::io(&dir, source))?;
        if !entry.path().join(PROJECT_FILE).is_file() {
            continue;
        }
        if let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|name| ProjectId::parse(name).ok())
        {
            ids.push(id);
        }
    }
    ids.sort();
    Ok(ids)
}

fn lab_path(root: &Path) -> PathBuf {
    root.join(LAB_FILE)
}

fn project_path(root: &Path, id: &ProjectId) -> PathBuf {
    root.join(PROJECTS_DIR).join(id.as_str()).join(PROJECT_FILE)
}

fn save<T>(path: &Path, value: &T) -> Result<(), ModelError>
where
    T: serde::Serialize,
{
    let text = toml::to_string(value)?;
    write_atomic(path, &text)
}

fn load<T>(path: &Path) -> Result<T, ModelError>
where
    T: DeserializeOwned,
{
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(source) if source.kind() == ErrorKind::NotFound => {
            return Err(ModelError::NotFound(path.to_path_buf()));
        }
        Err(source) => return Err(ModelError::io(path, source)),
    };
    toml::from_str(&text).map_err(|source| ModelError::InvalidToml {
        path: path.to_path_buf(),
        source,
    })
}

fn write_atomic(path: &Path, contents: &str) -> Result<(), ModelError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| ModelError::io(parent, source))?;

    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    fs::write(&tmp, contents).map_err(|source| ModelError::io(&tmp, source))?;
    fs::rename(&tmp, path).map_err(|source| ModelError::io(path, source))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU32, Ordering},
    };

    use super::*;
    use crate::{
        lab::{GithubAction, GithubGrant},
        project::{EnvProfile, ProjectStage, RepoSpec, RunRecipe},
    };

    /// Temporary root removed when the test ends.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("jgf-core-registry-{}-{suffix}", std::process::id()));
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

    fn sample_lab() -> LabConfig {
        LabConfig {
            name: "alpha".into(),
            github: GithubGrant {
                owners: vec!["acme".into()],
                actions: vec![GithubAction::CreateRepo, GithubAction::OpenPr],
                max_prs_per_day: 3,
                max_pushes_per_day: 10,
            },
        }
    }

    fn sample_project(id: &str) -> Project {
        Project {
            id: ProjectId::parse(id).expect("valid id"),
            name: "Example".into(),
            spec_path: PathBuf::from("/tmp/spec.md"),
            spec_sha256: Some("abc123".into()),
            repos: vec![RepoSpec {
                name: "app".into(),
                remote: Some("https://example.com/app.git".into()),
                default_branch: "main".into(),
            }],
            stage: ProjectStage::Mvp,
            env_profile: EnvProfile {
                allowed_keys: vec!["PATH".into(), "HOME".into()],
            },
            recipes: vec![RunRecipe {
                name: "build".into(),
                command: "cargo build".into(),
                cwd: Some("app".into()),
            }],
        }
    }

    #[test]
    fn lab_round_trip() {
        let root = TempRoot::new();
        let lab = sample_lab();

        save_lab(root.path(), &lab).expect("save lab");
        let loaded = load_lab(root.path()).expect("load lab");

        assert_eq!(loaded, lab);
    }

    #[test]
    fn project_round_trip() {
        let root = TempRoot::new();
        let project = sample_project("example");

        save_project(root.path(), &project).expect("save project");
        let loaded = load_project(root.path(), &project.id).expect("load project");

        assert_eq!(loaded, project);
    }

    #[test]
    fn list_projects_is_sorted_by_id() {
        let root = TempRoot::new();
        for id in ["beta", "alpha", "gamma"] {
            save_project(root.path(), &sample_project(id)).expect("save project");
        }
        fs::write(root.path().join(PROJECTS_DIR).join("loose.toml"), "").expect("loose file");

        let ids: Vec<String> = list_projects(root.path())
            .expect("list projects")
            .iter()
            .map(|id| id.as_str().to_string())
            .collect();

        assert_eq!(ids, ["alpha", "beta", "gamma"]);
    }

    #[test]
    fn list_projects_without_a_projects_dir_is_empty() {
        let root = TempRoot::new();
        assert!(
            list_projects(root.path())
                .expect("list projects")
                .is_empty()
        );
    }

    #[test]
    fn missing_files_return_not_found() {
        let root = TempRoot::new();
        assert!(matches!(
            load_lab(root.path()),
            Err(ModelError::NotFound(_))
        ));

        let id = ProjectId::parse("missing").expect("valid id");
        assert!(matches!(
            load_project(root.path(), &id),
            Err(ModelError::NotFound(_))
        ));
    }
}

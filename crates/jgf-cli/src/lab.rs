//! In-lab project commands.

use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

use clap::{Args, Subcommand};
use jgf_core::ProjectId;

use crate::mail;

const FONT_DIR: &str = "/font";

/// In-lab project operations.
#[derive(Args)]
pub struct LabArgs {
    #[command(subcommand)]
    pub action: LabAction,
}

/// Lab-level operations.
#[derive(Subcommand)]
pub enum LabAction {
    /// Manage projects.
    Project(ProjectArgs),
}

/// Project operations.
#[derive(Args)]
pub struct ProjectArgs {
    #[command(subcommand)]
    action: ProjectAction,
}

#[derive(Subcommand)]
enum ProjectAction {
    /// Create a project directory and copy its spec to SPEC.md.
    Create(ProjectCreateArgs),
    /// Add a project to the host scheduler queue.
    Queue(ProjectNameArgs),
}

#[derive(Args)]
struct ProjectCreateArgs {
    /// Project directory name.
    name: String,
    /// Spec file to copy into the project.
    #[arg(long)]
    spec: PathBuf,
}

#[derive(Args)]
struct ProjectNameArgs {
    /// Project identifier.
    name: String,
}

/// Executes a project operation.
pub fn run(args: LabArgs) -> Result<(), String> {
    let LabAction::Project(args) = args.action;
    match args.action {
        ProjectAction::Create(args) => {
            let project = create_project(Path::new(FONT_DIR), &args.name, &args.spec)?;
            println!("{}", project.display());
            Ok(())
        }
        ProjectAction::Queue(args) => {
            let project = ProjectId::parse(&args.name)
                .map_err(|error| format!("invalid project identifier: {error}"))?;
            let client = mail::client_from_env().map_err(|error| error.to_string())?;
            let added = client.enqueue(project).map_err(|error| error.to_string())?;
            println!("{}", if added { "queued" } else { "already queued" });
            Ok(())
        }
    }
}

fn create_project(font_dir: &Path, name: &str, spec: &Path) -> Result<PathBuf, String> {
    crate::roles::validate_identifier("font project name", name)?;
    let metadata = fs::symlink_metadata(font_dir)
        .map_err(|error| format!("could not inspect lab projects directory: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("lab projects directory must be a plain directory".to_string());
    }
    let font_dir = fs::canonicalize(font_dir)
        .map_err(|error| format!("could not resolve lab projects directory: {error}"))?;
    let project_dir = font_dir.join(name);
    fs::create_dir(&project_dir).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            format!("lab project {name:?} already exists")
        } else {
            format!("could not create lab project {name:?}: {error}")
        }
    })?;

    let spec_path = project_dir.join("SPEC.md");
    let copied = (|| {
        let mut source = File::open(spec)
            .map_err(|error| format!("could not open project spec {}: {error}", spec.display()))?;
        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&spec_path)
            .map_err(|error| {
                format!(
                    "could not create project spec {}: {error}",
                    spec_path.display()
                )
            })?;
        io::copy(&mut source, &mut destination).map_err(|error| {
            format!(
                "could not copy project spec to {}: {error}",
                spec_path.display()
            )
        })?;
        Ok::<(), String>(())
    })();
    if let Err(error) = copied {
        let _ = fs::remove_file(&spec_path);
        let _ = fs::remove_dir(&project_dir);
        return Err(error);
    }

    Ok(project_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    #[test]
    fn project_create_copies_spec() {
        let temp = tempfile::tempdir().expect("tempdir");
        let font = temp.path().join("font");
        fs::create_dir(&font).expect("font directory");
        let source = temp.path().join("input.md");
        fs::write(&source, "# Project spec\n").expect("source spec");

        let project = create_project(&font, "project-a", &source).expect("create project");

        assert_eq!(
            fs::read_to_string(project.join("SPEC.md")).expect("copied spec"),
            "# Project spec\n"
        );
    }

    #[test]
    fn project_create_rejects_traversal_and_existing_names() {
        let temp = tempfile::tempdir().expect("tempdir");
        let font = temp.path().join("font");
        fs::create_dir(&font).expect("font directory");
        let source = temp.path().join("input.md");
        fs::write(&source, "spec").expect("source spec");

        assert!(create_project(&font, "../outside", &source).is_err());
        fs::create_dir(font.join("existing")).expect("existing project");
        assert!(create_project(&font, "existing", &source).is_err());
        assert!(!temp.path().join("outside").exists());
    }

    #[cfg(unix)]
    #[test]
    fn project_create_rejects_project_and_font_symlinks() {
        let temp = tempfile::tempdir().expect("tempdir");
        let font = temp.path().join("font");
        fs::create_dir(&font).expect("font directory");
        let source = temp.path().join("input.md");
        fs::write(&source, "spec").expect("source spec");
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).expect("outside directory");
        symlink(&outside, font.join("linked")).expect("project symlink");
        assert!(create_project(&font, "linked", &source).is_err());

        let linked_font = temp.path().join("linked-font");
        symlink(&font, &linked_font).expect("font symlink");
        assert!(create_project(&linked_font, "project-a", &source).is_err());
    }
}

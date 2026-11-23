//! Secret values and the store that keeps them on the host.
//!
//! Secret values never enter briefs, chat or logs: [`Secret`] prints `[redacted]` and its
//! value is only reachable through [`Secret::expose`]. Stores are swappable through the
//! [`SecretStore`] trait.

use std::{
    fmt,
    fs::{self, DirBuilder, OpenOptions, Permissions},
    io::{ErrorKind, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    error::ModelError,
    project::{Project, ProjectId},
};

/// Text printed instead of a secret value.
const REDACTED: &str = "[redacted]";

/// Maximum length of a [`SecretKey`].
const MAX_KEY_LEN: usize = 64;

/// Directory holding all secret files inside a store root.
const SECRETS_DIR: &str = "secrets";
/// Directory holding lab-wide secrets.
const LAB_DIR: &str = "lab";
/// Directory holding one subdirectory per project.
const PROJECTS_DIR: &str = "projects";
/// Permission bits of directories owned by the store.
const DIR_MODE: u32 = 0o700;
/// Permission bits of secret files.
const FILE_MODE: u32 = 0o600;

/// A secret value kept on the host.
///
/// It cannot be serialized and both [`fmt::Debug`] and [`fmt::Display`] print
/// `[redacted]`, so it does not leak through logs or error messages.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    /// Wraps `value` as a secret.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret value. The only way to read it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

/// Where a secret value is stored.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SecretScope {
    /// Values shared by the whole lab.
    Lab,
    /// Values of one project.
    Project(ProjectId),
}

/// Name of a secret, following environment-variable rules.
///
/// It has 1 to 64 characters, starts with an uppercase letter, and otherwise contains only
/// uppercase letters, digits and `_`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SecretKey(String);

impl SecretKey {
    /// Parses and validates a secret key.
    pub fn parse(raw: &str) -> Result<Self, ModelError> {
        if !Self::is_valid(raw) {
            return Err(ModelError::InvalidSecretKey(raw.to_string()));
        }
        Ok(Self(raw.to_string()))
    }

    /// The key as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_valid(raw: &str) -> bool {
        !raw.is_empty()
            && raw.len() <= MAX_KEY_LEN
            && raw.starts_with(|c: char| c.is_ascii_uppercase())
            && raw
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    }
}

/// Storage of secret values, keyed by scope and name.
///
/// Later mechanisms may replace the implementation, so callers depend on this trait only.
pub trait SecretStore {
    /// The value stored for `key` in `scope`, or `None` when it is not set.
    fn get(&self, scope: &SecretScope, key: &SecretKey) -> Result<Option<Secret>, ModelError>;

    /// Stores `secret` under `key` in `scope`, replacing an existing value.
    fn set(&self, scope: &SecretScope, key: &SecretKey, secret: Secret) -> Result<(), ModelError>;

    /// Removes `key` from `scope`. Removing a missing key is not an error.
    fn remove(&self, scope: &SecretScope, key: &SecretKey) -> Result<(), ModelError>;

    /// Names of all keys set in `scope`, sorted.
    fn list_keys(&self, scope: &SecretScope) -> Result<Vec<SecretKey>, ModelError>;
}

/// A [`SecretStore`] that keeps one file per key under `<root>/secrets`.
///
/// Lab-wide values live in `<root>/secrets/lab`; project values in
/// `<root>/secrets/projects/<id>`. Directories are created with mode 0700 and files with
/// mode 0600.
pub struct FileSecretStore {
    root: PathBuf,
}

impl FileSecretStore {
    /// A store whose files live under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn scope_dir(&self, scope: &SecretScope) -> PathBuf {
        let secrets = self.root.join(SECRETS_DIR);
        match scope {
            SecretScope::Lab => secrets.join(LAB_DIR),
            SecretScope::Project(id) => secrets.join(PROJECTS_DIR).join(id.as_str()),
        }
    }

    fn ensure_scope_dir(&self, scope: &SecretScope) -> Result<PathBuf, ModelError> {
        let secrets = self.root.join(SECRETS_DIR);
        ensure_private_dir(&secrets)?;
        match scope {
            SecretScope::Lab => {
                let dir = secrets.join(LAB_DIR);
                ensure_private_dir(&dir)?;
                Ok(dir)
            }
            SecretScope::Project(id) => {
                let projects = secrets.join(PROJECTS_DIR);
                ensure_private_dir(&projects)?;
                let dir = projects.join(id.as_str());
                ensure_private_dir(&dir)?;
                Ok(dir)
            }
        }
    }
}

impl SecretStore for FileSecretStore {
    fn get(&self, scope: &SecretScope, key: &SecretKey) -> Result<Option<Secret>, ModelError> {
        let path = self.scope_dir(scope).join(key.as_str());
        match fs::read_to_string(&path) {
            Ok(value) => Ok(Some(Secret::new(value))),
            Err(source) if source.kind() == ErrorKind::NotFound => Ok(None),
            Err(source) => Err(ModelError::io(&path, source)),
        }
    }

    fn set(&self, scope: &SecretScope, key: &SecretKey, secret: Secret) -> Result<(), ModelError> {
        let dir = self.ensure_scope_dir(scope)?;
        write_atomic(&dir.join(key.as_str()), secret.expose())
    }

    fn remove(&self, scope: &SecretScope, key: &SecretKey) -> Result<(), ModelError> {
        let path = self.scope_dir(scope).join(key.as_str());
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == ErrorKind::NotFound => Ok(()),
            Err(source) => Err(ModelError::io(&path, source)),
        }
    }

    fn list_keys(&self, scope: &SecretScope) -> Result<Vec<SecretKey>, ModelError> {
        let dir = self.scope_dir(scope);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(ModelError::io(&dir, source)),
        };

        let mut keys = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| ModelError::io(&dir, source))?;
            let file_type = entry
                .file_type()
                .map_err(|source| ModelError::io(entry.path(), source))?;
            if !file_type.is_file() {
                continue;
            }
            if let Some(key) = entry
                .file_name()
                .to_str()
                .and_then(|name| SecretKey::parse(name).ok())
            {
                keys.push(key);
            }
        }
        keys.sort();
        Ok(keys)
    }
}

/// Environment values of `project` from `store`, sorted by key.
///
/// Only keys in the project env profile are considered, and only keys present in the project
/// scope are returned. A profile key that is not a valid [`SecretKey`] is an error.
pub fn env_for_project(
    store: &dyn SecretStore,
    project: &Project,
) -> Result<Vec<(SecretKey, Secret)>, ModelError> {
    let mut keys = Vec::with_capacity(project.env_profile.allowed_keys.len());
    for raw in &project.env_profile.allowed_keys {
        keys.push(SecretKey::parse(raw)?);
    }
    keys.sort();
    keys.dedup();

    let scope = SecretScope::Project(project.id.clone());
    let mut env = Vec::new();
    for key in keys {
        if let Some(secret) = store.get(&scope, &key)? {
            env.push((key, secret));
        }
    }
    Ok(env)
}

fn ensure_private_dir(path: &Path) -> Result<(), ModelError> {
    DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(path)
        .map_err(|source| ModelError::io(path, source))?;
    fs::set_permissions(path, Permissions::from_mode(DIR_MODE))
        .map_err(|source| ModelError::io(path, source))
}

fn write_atomic(path: &Path, value: &str) -> Result<(), ModelError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_private_dir(parent)?;

    let temp = temp_path(path);
    let written = write_temp(&temp, value)
        .and_then(|()| fs::rename(&temp, path).map_err(|source| ModelError::io(path, source)));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

fn write_temp(path: &Path, value: &str) -> Result<(), ModelError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .open(path)
        .map_err(|source| ModelError::io(path, source))?;
    file.write_all(value.as_bytes())
        .map_err(|source| ModelError::io(path, source))?;
    file.flush()
        .map_err(|source| ModelError::io(path, source))?;
    fs::set_permissions(path, Permissions::from_mode(FILE_MODE))
        .map_err(|source| ModelError::io(path, source))
}

/// Unique temporary sibling of `path`, so the rename stays on the same filesystem.
fn temp_path(path: &Path) -> PathBuf {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let suffix = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.{}.{suffix}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::BTreeMap};

    use super::*;
    use crate::project::{EnvProfile, ProjectStage};

    /// Temporary root removed when the test ends.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("jgf-core-secrets-{}-{suffix}", std::process::id()));
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

    /// In-memory [`SecretStore`] that proves stores are swappable.
    #[derive(Default)]
    struct InMemorySecretStore {
        values: RefCell<BTreeMap<(SecretScope, SecretKey), Secret>>,
    }

    impl SecretStore for InMemorySecretStore {
        fn get(&self, scope: &SecretScope, key: &SecretKey) -> Result<Option<Secret>, ModelError> {
            Ok(self
                .values
                .borrow()
                .get(&(scope.clone(), key.clone()))
                .cloned())
        }

        fn set(
            &self,
            scope: &SecretScope,
            key: &SecretKey,
            secret: Secret,
        ) -> Result<(), ModelError> {
            self.values
                .borrow_mut()
                .insert((scope.clone(), key.clone()), secret);
            Ok(())
        }

        fn remove(&self, scope: &SecretScope, key: &SecretKey) -> Result<(), ModelError> {
            self.values
                .borrow_mut()
                .remove(&(scope.clone(), key.clone()));
            Ok(())
        }

        fn list_keys(&self, scope: &SecretScope) -> Result<Vec<SecretKey>, ModelError> {
            let mut keys: Vec<SecretKey> = self
                .values
                .borrow()
                .keys()
                .filter(|(stored, _)| stored == scope)
                .map(|(_, key)| key.clone())
                .collect();
            keys.sort();
            Ok(keys)
        }
    }

    fn sample_project(id: &str, allowed: &[&str]) -> Project {
        Project {
            id: ProjectId::parse(id).expect("valid id"),
            name: "Example".into(),
            spec_path: PathBuf::from("/tmp/spec.md"),
            spec_sha256: None,
            repos: Vec::new(),
            stage: ProjectStage::Mvp,
            env_profile: EnvProfile {
                allowed_keys: allowed.iter().map(|key| key.to_string()).collect(),
            },
            recipes: Vec::new(),
        }
    }

    fn secret(value: &str) -> Secret {
        Secret::new(value)
    }

    fn key(raw: &str) -> SecretKey {
        SecretKey::parse(raw).expect("valid key")
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    #[test]
    fn debug_and_display_redact_the_value() {
        let value = "not-a-real-secret";
        let secret = secret(value);

        assert_eq!(format!("{secret:?}"), REDACTED);
        assert_eq!(format!("{secret}"), REDACTED);
        assert!(!format!("{secret:?}").contains(value));
        assert!(!format!("{secret}").contains(value));
        assert_eq!(secret.expose(), value);
    }

    #[test]
    fn failed_operations_do_not_leak_the_value() {
        let root = TempRoot::new();
        let blocked = root.path().join("blocked");
        fs::write(&blocked, "").expect("root file");

        let store = FileSecretStore::new(&blocked);
        let value = "not-a-real-secret";
        let key = key("API_TOKEN");
        let scope = SecretScope::Lab;

        let set_error = store
            .set(&scope, &key, secret(value))
            .expect_err("set must fail");
        assert!(!set_error.to_string().contains(value));

        let get_error = store.get(&scope, &key).expect_err("get must fail");
        assert!(!get_error.to_string().contains(value));

        let list_error = store.list_keys(&scope).expect_err("list must fail");
        assert!(!list_error.to_string().contains(value));
    }

    #[test]
    fn file_and_directory_permissions_are_private() {
        let root = TempRoot::new();
        let store = FileSecretStore::new(root.path());
        let key = key("API_TOKEN");

        store
            .set(&SecretScope::Lab, &key, secret("not-a-real-secret"))
            .expect("set lab secret");
        let lab_dir = root.path().join(SECRETS_DIR).join(LAB_DIR);
        assert_eq!(mode(&root.path().join(SECRETS_DIR)), DIR_MODE);
        assert_eq!(mode(&lab_dir), DIR_MODE);
        assert_eq!(mode(&lab_dir.join(key.as_str())), FILE_MODE);

        let project = sample_project("example", &[]);
        let project_scope = SecretScope::Project(project.id.clone());
        store
            .set(&project_scope, &key, secret("not-a-real-secret"))
            .expect("set project secret");
        let projects_dir = root.path().join(SECRETS_DIR).join(PROJECTS_DIR);
        let project_dir = projects_dir.join(project.id.as_str());
        assert_eq!(mode(&projects_dir), DIR_MODE);
        assert_eq!(mode(&project_dir), DIR_MODE);
        assert_eq!(mode(&project_dir.join(key.as_str())), FILE_MODE);
    }

    #[test]
    fn secret_key_parsing_accepts_only_env_style_names() {
        let too_long = "A".repeat(MAX_KEY_LEN + 1);
        for raw in ["lower", "1A", "", "A/B", "../X", too_long.as_str()] {
            assert!(
                SecretKey::parse(raw).is_err(),
                "accepted invalid key {raw:?}"
            );
        }

        for raw in ["A", "API_TOKEN", "A1"] {
            assert_eq!(key(raw).as_str(), raw);
        }
    }

    #[test]
    fn set_get_remove_round_trip() {
        let root = TempRoot::new();
        let store = FileSecretStore::new(root.path());
        let key = key("API_TOKEN");
        let scope = SecretScope::Lab;

        assert!(store.get(&scope, &key).expect("get").is_none());

        store
            .set(&scope, &key, secret("not-a-real-secret"))
            .expect("set");
        assert_eq!(
            store
                .get(&scope, &key)
                .expect("get")
                .expect("present")
                .expose(),
            "not-a-real-secret"
        );

        store.remove(&scope, &key).expect("remove");
        assert!(store.get(&scope, &key).expect("get").is_none());
        store.remove(&scope, &key).expect("remove missing key");
    }

    #[test]
    fn list_keys_is_sorted_and_names_only() {
        let root = TempRoot::new();
        let store = FileSecretStore::new(root.path());
        let scope = SecretScope::Lab;

        for name in ["ZETA", "ALPHA", "BETA", "DELTA"] {
            store
                .set(&scope, &key(name), secret("not-a-real-secret"))
                .expect("set lab secret");
        }
        let project = sample_project("example", &[]);
        store
            .set(
                &SecretScope::Project(project.id),
                &key("GAMMA"),
                secret("not-a-real-secret"),
            )
            .expect("set project secret");

        let names: Vec<String> = store
            .list_keys(&scope)
            .expect("list")
            .iter()
            .map(|key| key.as_str().to_string())
            .collect();
        assert_eq!(names, ["ALPHA", "BETA", "DELTA", "ZETA"]);
    }

    fn seed_and_check_env(store: &dyn SecretStore) {
        let project = sample_project("example", &["ZETA", "MISSING", "ALPHA"]);
        let scope = SecretScope::Project(project.id.clone());

        for (name, value) in [
            ("ALPHA", "not-a-real-secret-alpha"),
            ("ZETA", "not-a-real-secret-zeta"),
            ("EXTRA", "not-a-real-secret-extra"),
        ] {
            store
                .set(&scope, &key(name), secret(value))
                .expect("set project secret");
        }
        store
            .set(
                &SecretScope::Lab,
                &key("ALPHA"),
                secret("not-a-real-secret-lab"),
            )
            .expect("set lab secret");

        let env = env_for_project(store, &project).expect("env");
        let names: Vec<&str> = env.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(names, ["ALPHA", "ZETA"]);
        assert_eq!(env[0].1.expose(), "not-a-real-secret-alpha");
        assert_eq!(env[1].1.expose(), "not-a-real-secret-zeta");
    }

    #[test]
    fn env_for_project_filters_by_allowlist_and_presence() {
        let root = TempRoot::new();
        seed_and_check_env(&FileSecretStore::new(root.path()));
    }

    #[test]
    fn env_for_project_works_with_an_in_memory_store() {
        seed_and_check_env(&InMemorySecretStore::default());
    }

    #[test]
    fn env_for_project_rejects_invalid_profile_keys() {
        let store = InMemorySecretStore::default();
        let project = sample_project("example", &["lower"]);

        let error = env_for_project(&store, &project).expect_err("invalid profile key");
        assert!(matches!(error, ModelError::InvalidSecretKey(_)));
    }
}

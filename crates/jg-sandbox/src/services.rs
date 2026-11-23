//! Throwaway PostgreSQL and Redis instances for a scoped exec.
//!
//! A scoped exec gets a private `/tmp` tmpfs, so a service lives entirely below
//! `/tmp/jg-services/<project>`: its data directory, unix socket and log are
//! fixed paths inside that tmpfs and nothing is shared with another project.
//! PostgreSQL listens on a unix socket only (TCP disabled) and trusts local
//! socket connections; Redis also uses a unix socket and keeps no persistence.
//! Both hold only throwaway data for builds and tests inside a sandbox.
//!
//! [`ServiceBinaries::discover`] finds the host programs, and
//! [`ServiceBinaries::ro_binds`] returns the directories to mount read-only.
//! The command builders ([`ServiceSpec::prepare_commands`],
//! [`ServiceSpec::init_command`] and [`ServiceSpec::start_command`]) are pure:
//! they only build argv vectors, and the caller runs them through
//! [`crate::Backend::exec`] with the project scope. A missing program fails
//! closed when [`ServiceBinaries::require`] is called.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Result, SandboxError};
use crate::scope::{ExecScope, TMP_DIR, validate_project_name};

/// Name of the directory below the scope's private `/tmp` holding the services.
pub const SERVICES_DIR_NAME: &str = "jg-services";

/// TCP port PostgreSQL would use; the socket file keeps the same number.
pub const POSTGRES_PORT: u16 = 5432;

/// Socket file PostgreSQL creates in its socket directory.
pub const POSTGRES_SOCKET_NAME: &str = ".s.PGSQL.5432";

/// File name of the Redis unix socket.
pub const REDIS_SOCKET_NAME: &str = "redis.sock";

/// PostgreSQL superuser created by `initdb`.
pub const POSTGRES_USER: &str = "postgres";

/// Database PostgreSQL creates by default.
pub const POSTGRES_DATABASE: &str = "postgres";

/// Directories searched for service executables besides the versioned ones.
const SYSTEM_BIN_DIRS: [&str; 6] = [
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/local/sbin",
    "/usr/sbin",
    "/sbin",
];

/// Base directory of the versioned PostgreSQL installations.
const POSTGRES_LIB_DIR: &str = "/usr/lib/postgresql";

/// A service a scoped exec may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ServiceKind {
    /// PostgreSQL, on a unix socket only.
    Postgres,
    /// Redis, on a unix socket only and without persistence.
    Redis,
}

impl ServiceKind {
    /// Short name used in paths and error messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Redis => "redis",
        }
    }
}

/// One service to start for a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceSpec {
    /// Which service to run.
    pub kind: ServiceKind,
}

impl ServiceSpec {
    /// A spec for the given service.
    pub fn new(kind: ServiceKind) -> Self {
        Self { kind }
    }

    /// A PostgreSQL spec.
    pub fn postgres() -> Self {
        Self::new(ServiceKind::Postgres)
    }

    /// A Redis spec.
    pub fn redis() -> Self {
        Self::new(ServiceKind::Redis)
    }

    /// The fixed paths of this service inside `scope`'s private `/tmp`.
    pub fn paths(&self, scope: &ExecScope) -> Result<ServicePaths> {
        ServicePaths::for_scope(self.kind, scope)
    }

    /// Commands that create the service directories, to run before anything else.
    ///
    /// They run inside the sandbox, where `/tmp` is the scope's private tmpfs.
    pub fn prepare_commands(&self, paths: &ServicePaths) -> Result<Vec<Vec<String>>> {
        self.check(paths)?;
        let mut commands = vec![mkdir_argv(&paths.socket_dir, true)];
        if self.kind == ServiceKind::Redis {
            // PostgreSQL's `initdb` creates the (still missing) data directory.
            commands.push(mkdir_argv(&paths.data_dir, false));
        }
        Ok(commands)
    }

    /// The command that initialises the service, if it needs one.
    ///
    /// PostgreSQL needs `initdb`; Redis returns `None`.
    pub fn init_command(
        &self,
        paths: &ServicePaths,
        programs: &ServicePrograms,
    ) -> Result<Option<Vec<String>>> {
        self.check(paths)?;
        match programs {
            ServicePrograms::Postgres(postgres) if self.kind == ServiceKind::Postgres => {
                Ok(Some(postgres_initdb_argv(paths, postgres)))
            }
            ServicePrograms::Redis(_) if self.kind == ServiceKind::Redis => Ok(None),
            _ => Err(kind_mismatch(self.kind, programs.kind())),
        }
    }

    /// The command that starts the service in the background.
    pub fn start_command(
        &self,
        paths: &ServicePaths,
        programs: &ServicePrograms,
    ) -> Result<Vec<String>> {
        self.check(paths)?;
        match programs {
            ServicePrograms::Postgres(postgres) if self.kind == ServiceKind::Postgres => {
                Ok(postgres_start_argv(paths, postgres))
            }
            ServicePrograms::Redis(redis) if self.kind == ServiceKind::Redis => {
                Ok(redis_start_argv(paths, redis))
            }
            _ => Err(kind_mismatch(self.kind, programs.kind())),
        }
    }

    fn check(&self, paths: &ServicePaths) -> Result<()> {
        if paths.kind == self.kind {
            Ok(())
        } else {
            Err(service_error(format!(
                "paths for {} used with a {} service",
                paths.kind.name(),
                self.kind.name()
            )))
        }
    }
}

/// The fixed paths of one service instance inside a project scope.
///
/// Every path is an in-sandbox absolute path below `/tmp`, which is the
/// scope's private tmpfs. They depend only on the project name and the service
/// kind, so two projects never share a data directory or socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicePaths {
    /// Which service these paths belong to.
    pub kind: ServiceKind,
    /// The project key the paths were derived from.
    pub project: String,
    /// The service directory (`/tmp/jg-services/<project>/<service>`).
    pub root: PathBuf,
    /// The data directory of the service.
    pub data_dir: PathBuf,
    /// The directory holding the unix socket.
    pub socket_dir: PathBuf,
    /// The unix socket of the service.
    pub socket: PathBuf,
    /// The log file of the service.
    pub log: PathBuf,
    /// The pid file Redis writes; PostgreSQL keeps its own under `data_dir`.
    pub pid_file: Option<PathBuf>,
}

impl ServicePaths {
    /// The fixed paths of `kind` for `scope`.
    ///
    /// The project key is [`ExecScope::project`] when set, otherwise the file
    /// name of the worktree; both must be valid project names. Fails closed
    /// when the scope is invalid or names no usable project.
    pub fn for_scope(kind: ServiceKind, scope: &ExecScope) -> Result<Self> {
        scope.validate()?;
        let project = project_key(scope)?;
        let root = Path::new(TMP_DIR)
            .join(SERVICES_DIR_NAME)
            .join(&project)
            .join(kind.name());
        let (socket, pid_file) = match kind {
            ServiceKind::Postgres => (root.join("run").join(POSTGRES_SOCKET_NAME), None),
            ServiceKind::Redis => (
                root.join("run").join(REDIS_SOCKET_NAME),
                Some(root.join("redis.pid")),
            ),
        };
        Ok(Self {
            kind,
            project,
            data_dir: root.join("data"),
            socket_dir: root.join("run"),
            socket,
            log: root.join(format!("{}.log", kind.name())),
            pid_file,
            root,
        })
    }

    /// Connection environment for programs running inside the sandbox.
    ///
    /// PostgreSQL gets `DATABASE_URL`, `PGHOST`, `PGPORT`, `PGUSER` and
    /// `PGDATABASE` pointing at its private socket; Redis gets `REDIS_URL` as a
    /// `unix://` URL.
    pub fn env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        match self.kind {
            ServiceKind::Postgres => {
                let socket_dir = self.socket_dir.display().to_string();
                env.insert("PGHOST".to_string(), socket_dir.clone());
                env.insert("PGPORT".to_string(), POSTGRES_PORT.to_string());
                env.insert("PGUSER".to_string(), POSTGRES_USER.to_string());
                env.insert("PGDATABASE".to_string(), POSTGRES_DATABASE.to_string());
                env.insert(
                    "DATABASE_URL".to_string(),
                    format!("postgresql://{POSTGRES_USER}@/{POSTGRES_DATABASE}?host={socket_dir}"),
                );
            }
            ServiceKind::Redis => {
                env.insert(
                    "REDIS_URL".to_string(),
                    format!("unix://{}", self.socket.display()),
                );
            }
        }
        env
    }
}

/// The PostgreSQL programs found on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostgresBinaries {
    /// The `initdb` binary.
    pub initdb: PathBuf,
    /// The `pg_ctl` binary.
    pub pg_ctl: PathBuf,
    /// The `postgres` server binary.
    pub postgres: PathBuf,
}

/// The Redis programs found on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedisBinaries {
    /// The `redis-server` binary.
    pub redis_server: PathBuf,
}

/// The programs of one service, as required by the command builders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServicePrograms {
    /// The PostgreSQL programs.
    Postgres(PostgresBinaries),
    /// The Redis programs.
    Redis(RedisBinaries),
}

impl ServicePrograms {
    fn kind(&self) -> ServiceKind {
        match self {
            Self::Postgres(_) => ServiceKind::Postgres,
            Self::Redis(_) => ServiceKind::Redis,
        }
    }
}

/// The service programs discovered on the host.
///
/// A missing field means that program is not installed; [`Self::require`]
/// turns that into a fail-closed error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceBinaries {
    /// The `initdb` binary.
    pub initdb: Option<PathBuf>,
    /// The `pg_ctl` binary.
    pub pg_ctl: Option<PathBuf>,
    /// The `postgres` server binary.
    pub postgres: Option<PathBuf>,
    /// The `redis-server` binary.
    pub redis_server: Option<PathBuf>,
}

impl ServiceBinaries {
    /// Discover the programs in the host `PATH`, the versioned PostgreSQL
    /// directories and the system binary directories.
    pub fn discover() -> Self {
        Self::discover_in(&host_bin_dirs())
    }

    /// Discover the programs looking only in `dirs`, in order.
    pub fn discover_in(dirs: &[PathBuf]) -> Self {
        let dirs = dedup_paths(dirs.iter().cloned());
        Self {
            initdb: find_binary(&dirs, "initdb"),
            pg_ctl: find_binary(&dirs, "pg_ctl"),
            postgres: find_binary(&dirs, "postgres"),
            redis_server: find_binary(&dirs, "redis-server"),
        }
    }

    /// The PostgreSQL programs, when all of them were found.
    pub fn postgres(&self) -> Option<PostgresBinaries> {
        Some(PostgresBinaries {
            initdb: self.initdb.clone()?,
            pg_ctl: self.pg_ctl.clone()?,
            postgres: self.postgres.clone()?,
        })
    }

    /// The Redis programs, when they were found.
    pub fn redis(&self) -> Option<RedisBinaries> {
        Some(RedisBinaries {
            redis_server: self.redis_server.clone()?,
        })
    }

    /// The programs of `kind`, or an error naming every missing binary.
    pub fn require(&self, kind: ServiceKind) -> Result<ServicePrograms> {
        match kind {
            ServiceKind::Postgres => match self.postgres() {
                Some(postgres) => Ok(ServicePrograms::Postgres(postgres)),
                None => {
                    let mut missing = Vec::new();
                    if self.initdb.is_none() {
                        missing.push("initdb");
                    }
                    if self.pg_ctl.is_none() {
                        missing.push("pg_ctl");
                    }
                    if self.postgres.is_none() {
                        missing.push("postgres");
                    }
                    Err(service_error(format!(
                        "PostgreSQL binaries not found on the host: {}",
                        missing.join(", ")
                    )))
                }
            },
            ServiceKind::Redis => match self.redis() {
                Some(redis) => Ok(ServicePrograms::Redis(redis)),
                None => Err(service_error(
                    "Redis binary not found on the host: redis-server".to_string(),
                )),
            },
        }
    }

    /// The host directories to mount read-only: the default binds plus the
    /// installation root of every discovered program, without duplicates.
    ///
    /// A PostgreSQL program in `/usr/lib/postgresql/16/bin` exposes
    /// `/usr/lib/postgresql/16`, so `initdb` still finds its `share` directory.
    pub fn ro_binds(&self) -> Vec<PathBuf> {
        let mut binds = crate::spec::default_ro_binds();
        let mut roots: Vec<PathBuf> = self.binaries().into_iter().map(tool_root).collect();
        // Parents before children, so a broad root wins over a redundant bind.
        roots.sort_by_key(|path| path.components().count());
        for root in roots {
            if !root.is_dir() || binds.iter().any(|bind| root.starts_with(bind)) {
                continue;
            }
            binds.push(root);
        }
        binds
    }

    /// The binaries of every discovered program.
    fn binaries(&self) -> Vec<&Path> {
        [
            self.initdb.as_deref(),
            self.pg_ctl.as_deref(),
            self.postgres.as_deref(),
            self.redis_server.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

fn postgres_initdb_argv(paths: &ServicePaths, postgres: &PostgresBinaries) -> Vec<String> {
    vec![
        postgres.initdb.display().to_string(),
        "-D".to_string(),
        paths.data_dir.display().to_string(),
        "-U".to_string(),
        POSTGRES_USER.to_string(),
        "--auth-local=trust".to_string(),
        "--auth-host=reject".to_string(),
        "--encoding=UTF8".to_string(),
        "--no-locale".to_string(),
        "--no-sync".to_string(),
    ]
}

/// PostgreSQL options passed through `pg_ctl -o`.
///
/// TCP is disabled, the socket lives in the service's own directory and write
/// durability is off because the data directory is a throwaway tmpfs.
fn postgres_options(paths: &ServicePaths) -> String {
    format!(
        "-c listen_addresses='' -c unix_socket_directories={} \
         -c unix_socket_permissions=0700 -c port={POSTGRES_PORT} \
         -c fsync=off -c synchronous_commit=off",
        paths.socket_dir.display()
    )
}

fn postgres_start_argv(paths: &ServicePaths, postgres: &PostgresBinaries) -> Vec<String> {
    vec![
        postgres.pg_ctl.display().to_string(),
        "-D".to_string(),
        paths.data_dir.display().to_string(),
        "-l".to_string(),
        paths.log.display().to_string(),
        "-p".to_string(),
        postgres.postgres.display().to_string(),
        "-o".to_string(),
        postgres_options(paths),
        "-w".to_string(),
        "-t".to_string(),
        "60".to_string(),
        "start".to_string(),
    ]
}

fn redis_start_argv(paths: &ServicePaths, redis: &RedisBinaries) -> Vec<String> {
    let mut argv = vec![
        redis.redis_server.display().to_string(),
        "--daemonize".to_string(),
        "yes".to_string(),
        "--port".to_string(),
        "0".to_string(),
        "--unixsocket".to_string(),
        paths.socket.display().to_string(),
        "--unixsocketperm".to_string(),
        "700".to_string(),
        "--dir".to_string(),
        paths.data_dir.display().to_string(),
        "--save".to_string(),
        String::new(),
        "--appendonly".to_string(),
        "no".to_string(),
        "--logfile".to_string(),
        paths.log.display().to_string(),
    ];
    if let Some(pid_file) = &paths.pid_file {
        argv.push("--pidfile".to_string());
        argv.push(pid_file.display().to_string());
    }
    argv
}

fn mkdir_argv(dir: &Path, private: bool) -> Vec<String> {
    let mut argv = vec!["mkdir".to_string()];
    if private {
        argv.push("-m".to_string());
        argv.push("700".to_string());
    }
    argv.push("-p".to_string());
    argv.push(dir.display().to_string());
    argv
}

fn project_key(scope: &ExecScope) -> Result<String> {
    if let Some(project) = &scope.project {
        validate_project_name(project)?;
        return Ok(project.clone());
    }
    let name = scope
        .worktree
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| service_error("scope worktree has no usable project name".to_string()))?;
    validate_project_name(name)?;
    Ok(name.to_string())
}

fn kind_mismatch(expected: ServiceKind, found: ServiceKind) -> SandboxError {
    service_error(format!(
        "programs for {} used with a {} service",
        found.name(),
        expected.name()
    ))
}

fn service_error(reason: String) -> SandboxError {
    SandboxError::ExecFailed { reason }
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

fn dedup_paths(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut unique = Vec::new();
    for path in paths {
        push_unique(&mut unique, path);
    }
    unique
}

fn find_binary(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    dirs.iter()
        .map(|dir| dir.join(name))
        .find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// The installation prefix of a binary: its parent, or the parent of a `bin`
/// directory (`/usr/lib/postgresql/16/bin/initdb` belongs to
/// `/usr/lib/postgresql/16`).
fn tool_root(binary: &Path) -> PathBuf {
    let parent = binary.parent().unwrap_or(Path::new("/"));
    if parent.file_name().and_then(|name| name.to_str()) == Some("bin") {
        return parent
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| parent.to_path_buf());
    }
    parent.to_path_buf()
}

/// Directories that may hold a service executable.
fn host_bin_dirs() -> Vec<PathBuf> {
    let home = host_home();
    let mut dirs = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(
            std::env::split_paths(&path)
                .filter(|dir| dir.is_absolute())
                // Never take a service from the user's home: its install root
                // must not become a read-only bind.
                .filter(|dir| match &home {
                    Some(home) => !dir.starts_with(home),
                    None => true,
                }),
        );
    }
    dirs.extend(postgres_version_dirs());
    dirs.extend(SYSTEM_BIN_DIRS.iter().map(PathBuf::from));
    dedup_paths(dirs)
}

fn postgres_version_dirs() -> Vec<PathBuf> {
    postgres_version_dirs_in(Path::new(POSTGRES_LIB_DIR))
}

/// The `bin` directories of the versioned PostgreSQL installations below
/// `base`, newest first.
fn postgres_version_dirs_in(base: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    let mut versions: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.join("bin").is_dir())
        .collect();
    versions.sort_by_key(|path| std::cmp::Reverse(version_key(path)));
    versions
        .into_iter()
        .map(|version| version.join("bin"))
        .collect()
}

/// Numeric components of a version directory name (`16` -> `[16]`).
fn version_key(path: &Path) -> Vec<u64> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            name.split(|c: char| !c.is_ascii_digit())
                .filter(|part| !part.is_empty())
                .filter_map(|part| part.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn host_home() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    home.is_absolute().then_some(home)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::ScopeAccess;
    use std::os::unix::fs::PermissionsExt;

    fn scope(name: &str) -> ExecScope {
        ExecScope::new(ScopeAccess::ReadWrite, format!("/lab/projects/{name}")).with_project(name)
    }

    fn write_executable(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("bin dir");
        let path = dir.join(name);
        std::fs::write(&path, b"#!/bin/sh\n").expect("write executable");
        let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("permissions");
        path
    }

    fn has_pair(argv: &[String], first: &str, second: &str) -> bool {
        argv.windows(2)
            .any(|pair| pair[0] == first && pair[1] == second)
    }

    fn postgres_programs(bin: &Path) -> ServicePrograms {
        let binaries = ServiceBinaries {
            initdb: Some(bin.join("initdb")),
            pg_ctl: Some(bin.join("pg_ctl")),
            postgres: Some(bin.join("postgres")),
            redis_server: None,
        };
        binaries
            .require(ServiceKind::Postgres)
            .expect("postgres programs")
    }

    fn redis_programs(bin: &Path) -> ServicePrograms {
        let binaries = ServiceBinaries {
            redis_server: Some(bin.join("redis-server")),
            ..ServiceBinaries::default()
        };
        binaries
            .require(ServiceKind::Redis)
            .expect("redis programs")
    }

    #[test]
    fn discovery_finds_every_program() {
        let temp = tempfile::tempdir().expect("temp dir");
        let bin = temp.path().join("bin");
        for name in ["initdb", "pg_ctl", "postgres", "redis-server"] {
            write_executable(&bin, name);
        }

        let binaries = ServiceBinaries::discover_in(std::slice::from_ref(&bin));

        assert_eq!(binaries.initdb, Some(bin.join("initdb")));
        assert_eq!(binaries.pg_ctl, Some(bin.join("pg_ctl")));
        assert_eq!(binaries.postgres, Some(bin.join("postgres")));
        assert_eq!(binaries.redis_server, Some(bin.join("redis-server")));
    }

    #[test]
    fn non_executable_files_are_not_programs() {
        let temp = tempfile::tempdir().expect("temp dir");
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("bin dir");
        std::fs::write(bin.join("initdb"), b"not executable").expect("write file");

        let binaries = ServiceBinaries::discover_in(&[bin]);
        assert_eq!(binaries.initdb, None);
    }

    #[test]
    fn require_fails_closed_and_names_missing_programs() {
        let temp = tempfile::tempdir().expect("temp dir");
        let bin = temp.path().join("bin");
        write_executable(&bin, "redis-server");
        let binaries = ServiceBinaries::discover_in(std::slice::from_ref(&bin));

        let error = binaries
            .require(ServiceKind::Postgres)
            .expect_err("missing");
        let message = error.to_string();
        for name in ["initdb", "pg_ctl", "postgres"] {
            assert!(message.contains(name), "{message} must name {name}");
        }
        assert!(binaries.require(ServiceKind::Redis).is_ok());

        let empty = ServiceBinaries::default();
        assert!(empty.require(ServiceKind::Redis).is_err());
    }

    #[test]
    fn paths_are_fixed_per_project_and_service() {
        let postgres = ServiceSpec::postgres().paths(&scope("a")).expect("paths");
        let redis = ServiceSpec::redis().paths(&scope("a")).expect("paths");
        let other = ServiceSpec::postgres().paths(&scope("b")).expect("paths");

        assert_eq!(postgres.root, PathBuf::from("/tmp/jg-services/a/postgres"));
        assert_eq!(postgres.data_dir, postgres.root.join("data"));
        assert_eq!(postgres.socket_dir, postgres.root.join("run"));
        assert_eq!(
            postgres.socket,
            postgres.socket_dir.join(POSTGRES_SOCKET_NAME)
        );
        assert_eq!(postgres.pid_file, None);

        assert_eq!(redis.root, PathBuf::from("/tmp/jg-services/a/redis"));
        assert_eq!(redis.socket, redis.socket_dir.join(REDIS_SOCKET_NAME));
        assert_eq!(redis.pid_file, Some(redis.root.join("redis.pid")));

        assert_ne!(postgres.root, redis.root);
        assert_ne!(postgres.root, other.root);
        assert_eq!(other.project, "b");
    }

    #[test]
    fn paths_fall_back_to_the_worktree_name() {
        let unscoped = ExecScope::new(ScopeAccess::ReadWrite, "/lab/projects/demo");
        let paths = ServiceSpec::redis().paths(&unscoped).expect("paths");
        assert_eq!(paths.project, "demo");

        let unusable = ExecScope::new(ScopeAccess::ReadWrite, "/lab/projects/v1.2");
        assert!(ServiceSpec::redis().paths(&unusable).is_err());

        let outside = ExecScope::new(ScopeAccess::ReadWrite, "/etc");
        assert!(ServiceSpec::redis().paths(&outside).is_err());
    }

    #[test]
    fn prepare_commands_create_the_directories_inside_the_sandbox() {
        let paths = ServiceSpec::postgres().paths(&scope("a")).expect("paths");
        let commands = ServiceSpec::postgres()
            .prepare_commands(&paths)
            .expect("prepare");
        assert_eq!(
            commands,
            vec![vec![
                "mkdir".to_string(),
                "-m".to_string(),
                "700".to_string(),
                "-p".to_string(),
                paths.socket_dir.display().to_string(),
            ]]
        );

        let redis = ServiceSpec::redis().paths(&scope("a")).expect("paths");
        let commands = ServiceSpec::redis()
            .prepare_commands(&redis)
            .expect("prepare");
        assert_eq!(commands.len(), 2);
        assert!(commands[1].contains(&redis.data_dir.display().to_string()));
    }

    #[test]
    fn postgres_initdb_trusts_the_socket_only() {
        let bin = Path::new("/usr/lib/postgresql/16/bin");
        let paths = ServiceSpec::postgres().paths(&scope("a")).expect("paths");
        let argv = ServiceSpec::postgres()
            .init_command(&paths, &postgres_programs(bin))
            .expect("init")
            .expect("postgres has initdb");

        assert_eq!(argv[0], bin.join("initdb").display().to_string());
        assert!(has_pair(&argv, "-D", &paths.data_dir.display().to_string()));
        assert!(argv.contains(&"--auth-local=trust".to_string()));
        assert!(argv.contains(&"--auth-host=reject".to_string()));
        assert!(argv.contains(&"--no-sync".to_string()));
    }

    #[test]
    fn postgres_start_disables_tcp_and_names_the_socket_directory() {
        let bin = Path::new("/usr/lib/postgresql/16/bin");
        let paths = ServiceSpec::postgres().paths(&scope("a")).expect("paths");
        let argv = ServiceSpec::postgres()
            .start_command(&paths, &postgres_programs(bin))
            .expect("start");

        assert_eq!(argv[0], bin.join("pg_ctl").display().to_string());
        assert!(has_pair(
            &argv,
            "-p",
            &bin.join("postgres").display().to_string()
        ));
        assert!(has_pair(&argv, "-D", &paths.data_dir.display().to_string()));
        assert!(has_pair(&argv, "-l", &paths.log.display().to_string()));
        assert_eq!(argv.last().map(String::as_str), Some("start"));
        assert!(argv.contains(&"-w".to_string()));

        let options = argv
            .windows(2)
            .find(|pair| pair[0] == "-o")
            .map(|pair| pair[1].clone())
            .expect("-o options");
        assert!(options.contains("listen_addresses=''"), "{options}");
        assert!(
            options.contains(&format!(
                "unix_socket_directories={}",
                paths.socket_dir.display()
            )),
            "{options}"
        );
        assert!(options.contains("fsync=off"), "{options}");
        assert!(options.contains("synchronous_commit=off"), "{options}");
    }

    #[test]
    fn postgres_has_no_response_for_redis_programs() {
        let paths = ServiceSpec::postgres().paths(&scope("a")).expect("paths");
        let bin = Path::new("/opt/redis/bin");
        let error = ServiceSpec::postgres()
            .start_command(&paths, &redis_programs(bin))
            .expect_err("mismatched programs");
        assert!(error.to_string().contains("redis"));
    }

    #[test]
    fn redis_start_uses_a_socket_without_persistence() {
        let bin = Path::new("/usr/bin");
        let paths = ServiceSpec::redis().paths(&scope("a")).expect("paths");
        let argv = ServiceSpec::redis()
            .start_command(&paths, &redis_programs(bin))
            .expect("start");

        assert_eq!(argv[0], bin.join("redis-server").display().to_string());
        assert!(has_pair(&argv, "--daemonize", "yes"));
        assert!(has_pair(&argv, "--port", "0"));
        assert!(has_pair(
            &argv,
            "--unixsocket",
            &paths.socket.display().to_string()
        ));
        assert!(has_pair(
            &argv,
            "--dir",
            &paths.data_dir.display().to_string()
        ));
        assert!(has_pair(&argv, "--save", ""));
        assert!(has_pair(&argv, "--appendonly", "no"));
        assert!(has_pair(
            &argv,
            "--logfile",
            &paths.log.display().to_string()
        ));
        let pid_file = paths.pid_file.as_ref().expect("redis pid file");
        assert!(has_pair(
            &argv,
            "--pidfile",
            &pid_file.display().to_string()
        ));
    }

    #[test]
    fn redis_has_no_init_command() {
        let paths = ServiceSpec::redis().paths(&scope("a")).expect("paths");
        let bin = Path::new("/usr/bin");
        let command = ServiceSpec::redis()
            .init_command(&paths, &redis_programs(bin))
            .expect("init");
        assert_eq!(command, None);
    }

    #[test]
    fn paths_of_another_kind_are_rejected() {
        let redis_paths = ServiceSpec::redis().paths(&scope("a")).expect("paths");
        let bin = Path::new("/usr/lib/postgresql/16/bin");
        let error = ServiceSpec::postgres()
            .start_command(&redis_paths, &postgres_programs(bin))
            .expect_err("mismatched paths");
        assert!(error.to_string().contains("redis"));
    }

    #[test]
    fn connection_env_names_the_private_socket() {
        let postgres = ServiceSpec::postgres().paths(&scope("a")).expect("paths");
        let env = postgres.env();
        let socket_dir = postgres.socket_dir.display().to_string();
        assert_eq!(env["PGHOST"], socket_dir);
        assert_eq!(env["PGPORT"], POSTGRES_PORT.to_string());
        assert_eq!(env["PGUSER"], POSTGRES_USER);
        assert_eq!(env["PGDATABASE"], POSTGRES_DATABASE);
        assert_eq!(
            env["DATABASE_URL"],
            format!("postgresql://{POSTGRES_USER}@/{POSTGRES_DATABASE}?host={socket_dir}")
        );

        let redis = ServiceSpec::redis().paths(&scope("a")).expect("paths");
        let env = redis.env();
        assert_eq!(
            env["REDIS_URL"],
            format!("unix://{}", redis.socket.display())
        );
        assert!(!env.contains_key("DATABASE_URL"));
    }

    #[test]
    fn binds_expose_tool_roots_but_not_the_host_home() {
        let temp = tempfile::tempdir().expect("temp dir");
        let postgres_bin = temp.path().join("opt/postgres/bin");
        let redis_bin = temp.path().join("opt/redis/bin");
        write_executable(&postgres_bin, "initdb");
        write_executable(&postgres_bin, "pg_ctl");
        write_executable(&postgres_bin, "postgres");
        write_executable(&redis_bin, "redis-server");

        let binaries = ServiceBinaries::discover_in(&[postgres_bin, redis_bin]);
        let binds = binaries.ro_binds();

        assert!(binds.contains(&temp.path().join("opt/postgres")));
        assert!(binds.contains(&temp.path().join("opt/redis")));

        let mut unique = binds.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), binds.len(), "binds must be deduplicated");
    }

    #[test]
    fn binds_skip_missing_roots_and_roots_already_covered() {
        let temp = tempfile::tempdir().expect("temp dir");
        let binaries = ServiceBinaries {
            redis_server: Some(temp.path().join("missing/bin/redis-server")),
            ..ServiceBinaries::default()
        };
        let binds = binaries.ro_binds();
        assert!(!binds.iter().any(|bind| bind.starts_with(temp.path())));
        assert!(binds.contains(&PathBuf::from("/usr")));
    }

    #[test]
    fn versioned_postgres_directories_are_newest_first() {
        let temp = tempfile::tempdir().expect("temp dir");
        let base = temp.path().join("postgresql");
        for version in ["9.6", "16", "14"] {
            std::fs::create_dir_all(base.join(version).join("bin")).expect("version dir");
        }
        std::fs::create_dir_all(base.join("15")).expect("directory without bin");

        let dirs = postgres_version_dirs_in(&base);

        assert_eq!(
            dirs,
            vec![
                base.join("16/bin"),
                base.join("14/bin"),
                base.join("9.6/bin"),
            ]
        );
    }

    #[test]
    fn discovery_is_harmless_on_this_host() {
        let binaries = ServiceBinaries::discover();
        let home = host_home();
        for bind in binaries.ro_binds() {
            assert!(bind.is_absolute(), "{} must be absolute", bind.display());
            if let Some(home) = &home {
                assert!(
                    !bind.starts_with(home),
                    "{} must not expose the host home",
                    bind.display()
                );
            }
        }
    }
}

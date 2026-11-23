//! Starting and stopping the throwaway services of a project.
//!
//! A scoped exec gets its own nested PID namespace and its own private `/tmp`
//! (see [`crate::scope`]), which decides how a service can live:
//!
//! * A daemon never outlives the exec that started it. When the command of an
//!   exec exits, the kernel kills every process left in the exec's PID
//!   namespace, and the private tmpfs with the socket and data disappears with
//!   it. A service started by a short exec is therefore already gone when that
//!   exec returns.
//! * Two execs of the same project do not share `/tmp` either, so the socket
//!   paths of [`crate::services::ServicePaths`] only exist in the exec that
//!   created them.
//!
//! So [`start_services`] does not start the services "next to" the sandbox: it
//! hosts them in one long-lived *supervisor exec*, which creates the
//! directories, initialises and starts every service, waits for each one to
//! become ready and then blocks for as long as the [`ServiceSet`] exists. The
//! supervisor runs on its own thread and gets no short
//! [`crate::exec::ExecRequest`] timeout: a timeout would kill the whole tree,
//! services included. Its backstop timeout only outlives the session lifetime.
//!
//! The sandbox-wide `/run/jg/services/<project>` directory is shared by every
//! exec of a sandbox (unlike `/tmp`), so readiness and stop travel through it:
//! the supervisor writes a `<service>.ready` file per service, and
//! [`ServiceSet::stop`] creates `stop`, which makes the supervisor stop the
//! services and exit. The session directory is named after a random token from
//! `/dev/urandom`, so a sandbox process cannot guess it. The host reaches the
//! directory through the sandbox init's root (`/proc/<init-pid>/root/run/...`)
//! with `openat2(2)` and `RESOLVE_IN_ROOT | RESOLVE_NO_SYMLINKS`, and it opens
//! the marker files with `O_NOFOLLOW`: a path component the sandbox replaced by
//! a symlink makes the lookup fail instead of pointing the host at a file
//! outside the session. The session also ends after
//! [`DEFAULT_SERVICE_LIFETIME`], a backstop for a leaked set. Every failure is
//! an error: a missing program, a service that never becomes ready or a
//! supervisor that exits early fails the start.
//!
//! Because the supervisor owns its exec's private `/tmp`, [`ServiceSet::env`]
//! describes the connection environment of the services as seen by processes of
//! that session; no other exec of the sandbox can reach the sockets.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nix::fcntl::{OFlag, OpenHow, ResolveFlag, open, openat, openat2};
use nix::sys::signal::{Signal, kill};
use nix::sys::stat::{Mode, fstat};
use nix::unistd::{Pid, UnlinkatFlags, unlinkat};

use crate::backend::Backend;
use crate::error::{Result, SandboxError};
use crate::exec::{ExecOutput, ExecRequest};
use crate::handle::SandboxHandle;
use crate::namespace::NamespaceBackend;
use crate::scope::ExecScope;
use crate::services::{ServiceBinaries, ServiceKind, ServicePaths, ServicePrograms, ServiceSpec};

/// Sandbox-wide directory holding the control files of every service session.
///
/// It lives on the sandbox root tmpfs, which every exec of the sandbox shares,
/// and each project gets its own subdirectory.
pub const CONTROL_DIR: &str = "/run/jg/services";

/// How long a service may take to create its socket before the start fails.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a service session stays alive without being stopped.
///
/// It is a backstop against a leaked [`ServiceSet`]: the supervisor ends the
/// session on its own after this long instead of running forever.
pub const DEFAULT_SERVICE_LIFETIME: Duration = Duration::from_secs(30 * 60);

/// Extra time the supervisor exec gets past [`DEFAULT_SERVICE_LIFETIME`].
///
/// The supervisor script stops the session itself; this only bounds the exec so
/// a supervisor that misses its own deadline cannot keep a thread forever.
const LIFETIME_GRACE: Duration = Duration::from_secs(60);

/// Poll interval of the host while it waits for the ready markers.
const READY_POLL: Duration = Duration::from_millis(50);

/// Name of the file whose appearance ends the session.
const STOP_MARKER: &str = "stop";

/// Suffix of the file a service creates once it is ready.
const READY_SUFFIX: &str = ".ready";

/// How many random bytes name a service session.
const SESSION_TOKEN_BYTES: usize = 16;

/// Path resolution used for every host access below the sandbox root.
///
/// The directory is the root of the resolution and no symlink may appear in it,
/// so a symlink planted by the sandbox fails the lookup instead of redirecting
/// the host.
const SAFE_RESOLVE: ResolveFlag =
    ResolveFlag::RESOLVE_IN_ROOT.union(ResolveFlag::RESOLVE_NO_SYMLINKS);

/// Flags of the stop marker: create it fresh and never follow a final symlink.
///
/// `O_NONBLOCK` keeps a planted FIFO from blocking the host thread when the
/// marker is opened.
const STOP_FLAGS: OFlag = OFlag::O_WRONLY
    .union(OFlag::O_CREAT)
    .union(OFlag::O_EXCL)
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_NONBLOCK)
    .union(OFlag::O_CLOEXEC);

/// How often the host retries after removing a planted stop entry.
const STOP_ATTEMPTS: usize = 3;

/// How long the host waits for a forcefully stopped supervisor exec to end.
///
/// The kill itself is immediate; this only bounds the wait for the exec
/// thread to unwind, so a failed stop can never wait out the session backstop.
const FORCE_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Poll interval while waiting for a forcefully stopped exec.
const FORCE_STOP_POLL: Duration = Duration::from_millis(20);

/// One running set of services of a project.
///
/// The set owns the supervisor exec that hosts the services: they run until
/// [`Self::stop`] is called (or the set is dropped, which stops them on a best
/// effort and ignores the outcome). The services are throwaway: their data
/// lives in the supervisor's private `/tmp` and disappears with it.
pub struct ServiceSet {
    project: String,
    paths: Vec<ServicePaths>,
    env: BTreeMap<String, String>,
    control_dir: ControlDir,
    supervisor: Option<Supervisor>,
}

impl ServiceSet {
    /// The project the services belong to.
    pub fn project(&self) -> &str {
        &self.project
    }

    /// The fixed paths of every started service.
    pub fn paths(&self) -> &[ServicePaths] {
        &self.paths
    }

    /// Connection environment of the services, as seen inside the session.
    ///
    /// It holds the variables of every service: `DATABASE_URL`, `PGHOST`,
    /// `PGPORT`, `PGUSER` and `PGDATABASE` for PostgreSQL, and `REDIS_URL` for
    /// Redis.
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    /// Stop the services and wait for the supervisor exec to end.
    ///
    /// The remaining processes of the session are released by the kernel when
    /// the supervisor's PID namespace init exits, so no service survives.
    pub fn stop(mut self) -> Result<()> {
        self.shutdown()
    }

    fn ready(&self) -> bool {
        self.paths
            .iter()
            .all(|paths| self.control_dir.is_ready(paths.kind))
    }

    fn shutdown(&mut self) -> Result<()> {
        let Some(mut supervisor) = self.supervisor.take() else {
            return Ok(());
        };
        let stop_error = match self.control_dir.request_stop() {
            Ok(()) => None,
            Err(source) => Some(SandboxError::io(self.control_dir.path(), source)),
        };
        let mut supervisor_error = None;
        if stop_error.is_some() {
            // The sandbox removed the stop channel: kill the supervisor exec
            // instead of joining a thread that waits out the session lifetime.
            // Its own result is a failure by construction, so it is ignored.
            supervisor.terminate();
            let _ = supervisor.take_finished_result();
        } else if let Some(result) = supervisor.take_result() {
            match result {
                Err(error) => supervisor_error = Some(error),
                Ok(output) if !is_success(&output) => {
                    supervisor_error = Some(service_error(format!(
                        "service supervisor failed: {}",
                        status_text(&output)
                    )));
                }
                Ok(_) => {}
            }
        }
        let kinds: Vec<_> = self.paths.iter().map(|paths| paths.kind).collect();
        self.control_dir.cleanup(&kinds);
        match supervisor_error.or(stop_error) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for ServiceSet {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

impl std::fmt::Debug for ServiceSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceSet")
            .field("project", &self.project)
            .field("paths", &self.paths)
            .field("env", &self.env)
            .field("control_dir", &self.control_dir)
            .finish_non_exhaustive()
    }
}

/// Start the services of `scope` and return the running set.
///
/// The host programs are discovered with [`ServiceBinaries::discover`]; use
/// [`start_services_with`] to pass programs discovered elsewhere. Fails closed
/// when a program is missing, when a service does not become ready within
/// [`DEFAULT_READY_TIMEOUT`] or when the supervisor exec exits early.
pub fn start_services(
    backend: &NamespaceBackend,
    handle: &SandboxHandle,
    scope: &ExecScope,
    specs: &[ServiceSpec],
) -> Result<ServiceSet> {
    start_services_with(backend, handle, scope, specs, &ServiceBinaries::discover())
}

/// Start the services of `scope` with explicit host programs.
///
/// Same as [`start_services`], but the caller provides the programs instead of
/// having them discovered in the host `PATH`; it is also what tests use to run
/// fake daemons.
pub fn start_services_with(
    backend: &NamespaceBackend,
    handle: &SandboxHandle,
    scope: &ExecScope,
    specs: &[ServiceSpec],
    binaries: &ServiceBinaries,
) -> Result<ServiceSet> {
    if specs.is_empty() {
        return Err(service_error(
            "start_services needs at least one service".to_string(),
        ));
    }
    scope.validate()?;
    reject_duplicates(specs)?;

    let mut paths = Vec::with_capacity(specs.len());
    let mut programs = Vec::with_capacity(specs.len());
    for spec in specs {
        paths.push(spec.paths(scope)?);
        programs.push(binaries.require(spec.kind)?);
    }

    let project = paths[0].project.clone();
    let sandbox_control_dir = control_dir(&project)?;
    let control_dir = ControlDir::new(handle, &sandbox_control_dir);
    let mut env = BTreeMap::new();
    for path in &paths {
        env.extend(path.env());
    }
    let script = supervisor_script(specs, &paths, &programs, &sandbox_control_dir)?;
    let request = scripted_request(&script, scope, DEFAULT_SERVICE_LIFETIME + LIFETIME_GRACE);

    let mut set = ServiceSet {
        project,
        paths,
        env,
        control_dir,
        supervisor: Some(Supervisor::spawn(
            backend,
            handle,
            request,
            &sandbox_control_dir,
        )),
    };
    if let Err(error) = wait_ready(&mut set) {
        let _ = set.shutdown();
        return Err(error);
    }
    Ok(set)
}

/// The supervisor exec that hosts the services of one set.
///
/// It owns the thread blocked in [`Backend::exec`]; the exec carries only the
/// session backstop timeout, so only the stop marker (or the session lifetime)
/// ends it early.
struct Supervisor {
    join: Option<JoinHandle<Result<ExecOutput>>>,
    init_pid: i32,
    control_dir: String,
}

impl Supervisor {
    fn spawn(
        backend: &NamespaceBackend,
        handle: &SandboxHandle,
        request: ExecRequest,
        control_dir: &Path,
    ) -> Self {
        let backend = *backend;
        let init_pid = handle.init_pid();
        let control_dir = path_text(control_dir);
        let handle = handle.clone();
        let join = thread::spawn(move || backend.exec(&handle, &request));
        Self {
            join: Some(join),
            init_pid,
            control_dir,
        }
    }

    fn finished(&self) -> bool {
        self.join.as_ref().is_some_and(|join| join.is_finished())
    }

    /// Join the supervisor thread and return its exec result, if it is done.
    fn take_result(&mut self) -> Option<Result<ExecOutput>> {
        let join = self.join.take()?;
        Some(join.join().unwrap_or_else(|_| {
            Err(service_error(
                "the service supervisor thread panicked".to_string(),
            ))
        }))
    }

    /// Take the exec result only when the thread has already finished.
    ///
    /// Joining a supervisor that still awaits the session backstop would block
    /// the caller for half an hour, so a set that was forcefully stopped lets
    /// the thread finish on its own instead.
    fn take_finished_result(&mut self) -> Option<Result<ExecOutput>> {
        if self.finished() {
            self.take_result()
        } else {
            None
        }
    }

    /// Kill the exec that runs the supervisor script, without the stop marker.
    ///
    /// The sandbox can delete or replace the session directory, so the host
    /// cannot always place the stop marker. It cannot take back the process
    /// that runs the supervisor script, though: the script names the session
    /// directory, and killing that shell makes its PID 1 exit, which makes the
    /// kernel kill every process of the exec.
    fn terminate(&mut self) {
        let deadline = Instant::now() + FORCE_STOP_TIMEOUT;
        loop {
            self.kill_supervisor_processes();
            if self.finished() || Instant::now() >= deadline {
                return;
            }
            thread::sleep(FORCE_STOP_POLL);
        }
    }

    /// Signal every process of this sandbox that runs the supervisor script.
    fn kill_supervisor_processes(&self) {
        let Some(sandbox_user_ns) = namespace_inode(self.init_pid, "user") else {
            return;
        };
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            if pid == std::process::id() as i32 {
                continue;
            }
            if namespace_inode(pid, "user") != Some(sandbox_user_ns) {
                continue;
            }
            if !cmdline_contains(pid, &self.control_dir) {
                continue;
            }
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }
}

/// Inode of one namespace of a host pid, or `None` when it cannot be read.
fn namespace_inode(pid: i32, name: &str) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;

    std::fs::metadata(format!("/proc/{pid}/ns/{name}"))
        .ok()
        .map(|meta| meta.ino())
}

/// Whether the command line of a host pid contains `needle`.
fn cmdline_contains(pid: i32, needle: &str) -> bool {
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let needle = needle.as_bytes();
    !needle.is_empty() && cmdline.windows(needle.len()).any(|window| window == needle)
}

/// Fail the start as soon as the supervisor exits, and once the ready markers
/// of every service appear.
fn wait_ready(set: &mut ServiceSet) -> Result<()> {
    let started = Instant::now();
    loop {
        let finished = set
            .supervisor
            .as_ref()
            .is_some_and(|supervisor| supervisor.finished());
        if finished {
            let supervisor = set
                .supervisor
                .as_mut()
                .expect("a finished supervisor is still part of the set");
            let result = supervisor
                .take_result()
                .expect("a finished supervisor still holds its thread");
            return Err(match result {
                Err(error) => error,
                Ok(output) => service_error(format!(
                    "service supervisor exited before its services were ready: {}",
                    status_text(&output)
                )),
            });
        }

        if set.ready() {
            return Ok(());
        }
        if started.elapsed() >= DEFAULT_READY_TIMEOUT {
            return Err(service_error(format!(
                "services for project {:?} did not become ready within {}s",
                set.project,
                DEFAULT_READY_TIMEOUT.as_secs()
            )));
        }
        thread::sleep(READY_POLL);
    }
}

fn reject_duplicates(specs: &[ServiceSpec]) -> Result<()> {
    for (index, spec) in specs.iter().enumerate() {
        if specs[..index].iter().any(|other| other.kind == spec.kind) {
            return Err(service_error(format!(
                "{} requested more than once",
                spec.kind.name()
            )));
        }
    }
    Ok(())
}

/// The supervisor script: prepare, initialise, start, wait, then block.
fn supervisor_script(
    specs: &[ServiceSpec],
    paths: &[ServicePaths],
    programs: &[ServicePrograms],
    control_dir: &Path,
) -> Result<String> {
    let mut script = String::from("set -eu\numask 077\n");
    script.push_str(&format!(
        "mkdir -p -m 700 {}\n",
        shell_quote(&path_text(control_dir))
    ));
    for ((spec, paths), programs) in specs.iter().zip(paths).zip(programs) {
        for command in spec.prepare_commands(paths)? {
            script.push_str(&command_line(&command));
            script.push('\n');
        }
        if let Some(command) = spec.init_command(paths, programs)? {
            script.push_str(&command_line(&command));
            script.push('\n');
        }
        script.push_str(&command_line(&spec.start_command(paths, programs)?));
        script.push('\n');
        script.push_str(&readiness_wait(spec.kind, paths));
        script.push_str(&liveness_check(spec.kind, paths, programs));
        script.push_str(&format!(
            ": > {}\n",
            shell_quote(&path_text(&ready_marker(control_dir, spec.kind)))
        ));
    }
    script.push_str(&format!(
        "i=0\nwhile [ ! -e {} ] && [ \"$i\" -lt {} ]; do\n  i=$((i + 1))\n  sleep 0.2\ndone\n",
        shell_quote(&path_text(&stop_marker(control_dir))),
        ticks(DEFAULT_SERVICE_LIFETIME, 200)
    ));
    for (paths, programs) in paths.iter().zip(programs) {
        script.push_str(&stop_command(paths, programs));
    }
    script.push_str("exit 0\n");
    Ok(script)
}

/// Wait for the socket of one service, or fail the supervisor.
fn readiness_wait(kind: ServiceKind, paths: &ServicePaths) -> String {
    format!(
        "i=0\nready=0\nwhile [ \"$i\" -lt {ticks} ]; do\n  if [ -e {socket} ]; then\n    ready=1\n    break\n  fi\n  i=$((i + 1))\n  sleep 0.1\ndone\nif [ \"$ready\" -ne 1 ]; then\n  echo {message} >&2\n  exit 1\nfi\n",
        ticks = ticks(DEFAULT_READY_TIMEOUT, 100),
        socket = shell_quote(&path_text(&paths.socket)),
        message = shell_quote(&format!("jg-sandbox: {} did not become ready", kind.name())),
    )
}

/// Check that the service is still running once its socket appeared.
fn liveness_check(kind: ServiceKind, paths: &ServicePaths, programs: &ServicePrograms) -> String {
    match programs {
        ServicePrograms::Postgres(postgres) => format!(
            "if ! {} -D {} status >/dev/null 2>&1; then\n  echo {message} >&2\n  exit 1\nfi\n",
            shell_quote(&path_text(&postgres.pg_ctl)),
            shell_quote(&path_text(&paths.data_dir)),
            message = shell_quote(&format!(
                "jg-sandbox: {} exited after it was ready",
                kind.name()
            )),
        ),
        ServicePrograms::Redis(_) => match &paths.pid_file {
            Some(pid_file) => format!(
                "pid=$(cat {} 2>/dev/null) || pid=\nif [ -z \"$pid\" ] || ! kill -0 \"$pid\" 2>/dev/null; then\n  echo {message} >&2\n  exit 1\nfi\n",
                shell_quote(&path_text(pid_file)),
                message = shell_quote(&format!(
                    "jg-sandbox: {} exited after it was ready",
                    kind.name()
                )),
            ),
            None => String::new(),
        },
    }
}

/// Ask one service to stop; the kernel reaps whatever is left afterwards.
fn stop_command(paths: &ServicePaths, programs: &ServicePrograms) -> String {
    match programs {
        ServicePrograms::Postgres(postgres) => format!(
            "{} -D {} -m immediate -W stop >/dev/null 2>&1 || true\n",
            shell_quote(&path_text(&postgres.pg_ctl)),
            shell_quote(&path_text(&paths.data_dir)),
        ),
        ServicePrograms::Redis(_) => match &paths.pid_file {
            Some(pid_file) => format!(
                "pid=$(cat {} 2>/dev/null) || pid=\nif [ -n \"$pid\" ]; then\n  kill \"$pid\" 2>/dev/null || true\nfi\n",
                shell_quote(&path_text(pid_file)),
            ),
            None => String::new(),
        },
    }
}

fn scripted_request(script: &str, scope: &ExecScope, timeout: Duration) -> ExecRequest {
    ExecRequest::new(vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        script.to_string(),
    ])
    .with_scope(scope.clone())
    .with_timeout(timeout)
}

/// The control directory of one session, as seen inside the sandbox.
fn control_dir(project: &str) -> Result<PathBuf> {
    Ok(Path::new(CONTROL_DIR).join(project).join(session_token()?))
}

/// The init's root as the host reaches it.
fn host_root(handle: &SandboxHandle) -> PathBuf {
    PathBuf::from(format!("/proc/{}/root", handle.init_pid()))
}

/// An unguessable token that keeps two sessions of the same project apart.
fn session_token() -> Result<String> {
    use std::io::Read;

    let mut bytes = [0u8; SESSION_TOKEN_BYTES];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|source| SandboxError::io("/dev/urandom", source))?;
    let hex = b"0123456789abcdef";
    let mut token = String::with_capacity(SESSION_TOKEN_BYTES * 2);
    for byte in bytes {
        token.push(hex[(byte >> 4) as usize] as char);
        token.push(hex[(byte & 0x0f) as usize] as char);
    }
    Ok(token)
}

fn ready_name(kind: ServiceKind) -> String {
    format!("{}{READY_SUFFIX}", kind.name())
}

fn ready_marker(control_dir: &Path, kind: ServiceKind) -> PathBuf {
    control_dir.join(ready_name(kind))
}

fn stop_marker(control_dir: &Path) -> PathBuf {
    control_dir.join(STOP_MARKER)
}

/// The host's view of one session's control directory inside the sandbox.
///
/// Every access resolves the relative path below the init's root with
/// [`SAFE_RESOLVE`], and the marker files are opened with `O_NOFOLLOW`, so a
/// symlink the sandbox planted at any component makes the operation fail
/// instead of touching its target.
#[derive(Debug)]
struct ControlDir {
    root: PathBuf,
    relative: PathBuf,
}

impl ControlDir {
    fn new(handle: &SandboxHandle, sandbox_path: &Path) -> Self {
        let relative = sandbox_path.strip_prefix("/").unwrap_or(sandbox_path);
        Self {
            root: host_root(handle),
            relative: relative.to_path_buf(),
        }
    }

    /// The directory as the host would display it.
    fn path(&self) -> PathBuf {
        self.root.join(&self.relative)
    }

    /// Open the init's root; `/proc/<pid>/root` is the host's only entry point.
    fn open_root(&self) -> io::Result<OwnedFd> {
        open(
            &self.root,
            OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(errno_io)
    }

    /// Resolve one path below the sandbox root without following any symlink.
    fn resolve(&self, path: &Path) -> io::Result<OwnedFd> {
        let root = self.open_root()?;
        openat2(
            root,
            path,
            OpenHow::new()
                .flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC)
                .resolve(SAFE_RESOLVE),
        )
        .map_err(errno_io)
    }

    /// Open the session directory itself, refusing symlinks on the way.
    fn open_dir(&self) -> io::Result<OwnedFd> {
        self.resolve(&self.relative)
    }

    /// Whether a service's ready marker is a real regular file in the session.
    ///
    /// Anything the sandbox planted at the marker name, such as a symlink or a
    /// directory, is not trusted and counts as "not ready". `O_PATH` looks the
    /// name up without opening it for reading, so a FIFO planted at the marker
    /// name cannot block the host thread in `open`.
    fn is_ready(&self, kind: ServiceKind) -> bool {
        let Some(dir) = self.open_dir().ok() else {
            return false;
        };
        let name = ready_name(kind);
        let marker = openat(
            &dir,
            name.as_str(),
            OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        );
        match marker {
            Ok(marker) => fstat(&marker)
                .map(|stat| stat.st_mode & libc::S_IFMT == libc::S_IFREG)
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    /// Ask the supervisor to stop by creating the stop marker.
    ///
    /// A planted symlink is removed (never followed) and replaced by a real
    /// file; an existing directory also ends the session and is left alone.
    fn request_stop(&self) -> io::Result<()> {
        let dir = self.open_dir()?;
        for _ in 0..STOP_ATTEMPTS {
            match openat(
                &dir,
                STOP_MARKER,
                STOP_FLAGS,
                Mode::from_bits_truncate(0o600),
            ) {
                Ok(_) => return Ok(()),
                Err(nix::errno::Errno::EEXIST) => {
                    // Remove whatever occupies the name. `unlinkat` never
                    // follows the final component, so a planted symlink is
                    // removed instead of its target.
                    match unlinkat(&dir, STOP_MARKER, UnlinkatFlags::NoRemoveDir) {
                        Ok(()) | Err(nix::errno::Errno::ENOENT) => {}
                        // A directory at the name already makes the
                        // supervisor's `[ -e stop ]` true.
                        Err(nix::errno::Errno::EISDIR) => return Ok(()),
                        Err(error) => return Err(errno_io(error)),
                    }
                }
                Err(error) => return Err(errno_io(error)),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "cannot place the stop marker",
        ))
    }

    /// Best-effort removal of the session directory and the markers we created.
    ///
    /// Only known names are unlinked through the resolved directory fd, so a
    /// planted symlink is removed instead of its target; `rmdir` never follows
    /// a final symlink either.
    fn cleanup(&self, kinds: &[ServiceKind]) {
        if let Ok(dir) = self.open_dir() {
            let _ = unlinkat(&dir, STOP_MARKER, UnlinkatFlags::NoRemoveDir);
            for kind in kinds {
                let name = ready_name(*kind);
                let _ = unlinkat(&dir, name.as_str(), UnlinkatFlags::NoRemoveDir);
            }
        }
        let (Some(parent), Some(name)) = (self.relative.parent(), self.relative.file_name()) else {
            return;
        };
        let Ok(parent) = self.resolve(parent) else {
            return;
        };
        let _ = unlinkat(&parent, name, UnlinkatFlags::RemoveDir);
    }
}

fn errno_io(error: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

fn ticks(duration: Duration, millis: u64) -> u64 {
    (duration.as_millis() as u64).div_ceil(millis).max(1)
}

fn command_line(argv: &[String]) -> String {
    argv.iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Quote one word for a POSIX shell.
fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

fn path_text(path: &Path) -> String {
    path.display().to_string()
}

fn is_success(output: &ExecOutput) -> bool {
    output.exit_code == Some(0) && !output.timed_out
}

fn status_text(output: &ExecOutput) -> String {
    let status = if output.timed_out {
        "timed out".to_string()
    } else if let Some(code) = output.exit_code {
        format!("exit code {code}")
    } else if let Some(signal) = output.signal {
        format!("signal {signal}")
    } else {
        "unknown status".to_string()
    };
    let stderr = output.stderr_text();
    let stderr = stderr.trim();
    if stderr.is_empty() {
        status
    } else {
        format!("{status}: {stderr}")
    }
}

fn service_error(reason: String) -> SandboxError {
    SandboxError::ExecFailed { reason }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::ScopeAccess;
    use crate::spec::SandboxSpec;

    fn scope(name: &str) -> ExecScope {
        ExecScope::new(ScopeAccess::ReadWrite, format!("/lab/{name}"))
    }

    fn handle() -> SandboxHandle {
        SandboxHandle::new(SandboxSpec::new("/tmp/jg-test/state", "/lab"), 4242, None)
    }

    fn postgres_binaries() -> ServiceBinaries {
        ServiceBinaries {
            initdb: Some(PathBuf::from("/usr/lib/postgresql/16/bin/initdb")),
            pg_ctl: Some(PathBuf::from("/usr/lib/postgresql/16/bin/pg_ctl")),
            postgres: Some(PathBuf::from("/usr/lib/postgresql/16/bin/postgres")),
            ..ServiceBinaries::default()
        }
    }

    fn redis_binaries() -> ServiceBinaries {
        ServiceBinaries {
            redis_server: Some(PathBuf::from("/usr/bin/redis-server")),
            ..ServiceBinaries::default()
        }
    }

    fn both_binaries() -> ServiceBinaries {
        ServiceBinaries {
            redis_server: Some(PathBuf::from("/usr/bin/redis-server")),
            ..postgres_binaries()
        }
    }

    fn build(specs: &[ServiceSpec], binaries: &ServiceBinaries) -> String {
        let scope = scope("demo");
        let paths: Vec<_> = specs
            .iter()
            .map(|spec| spec.paths(&scope).expect("paths"))
            .collect();
        let programs: Vec<_> = specs
            .iter()
            .map(|spec| binaries.require(spec.kind).expect("programs"))
            .collect();
        let control = Path::new(CONTROL_DIR).join("demo").join("token");
        supervisor_script(specs, &paths, &programs, &control).expect("script")
    }

    #[test]
    fn control_dir_is_per_project_and_per_session() {
        let first = control_dir("a").expect("token");
        let second = control_dir("a").expect("token");
        let other = control_dir("b").expect("token");
        assert!(first.starts_with(CONTROL_DIR));
        assert!(first.parent().expect("parent").ends_with("a"));
        assert_ne!(first, second, "sessions must not share control files");
        assert_ne!(first.parent(), other.parent());
    }

    #[test]
    fn session_token_is_unguessable_and_hex() {
        let first = session_token().expect("token");
        let second = session_token().expect("token");
        assert_eq!(first.len(), SESSION_TOKEN_BYTES * 2);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, second);
        assert!(
            !first.contains(&std::process::id().to_string()),
            "the token must not be derived from the host pid"
        );
    }

    #[test]
    fn host_root_goes_through_the_init_root() {
        let in_sandbox = control_dir("demo").expect("token");
        let control = ControlDir::new(&handle(), &in_sandbox);
        assert_eq!(control.root, PathBuf::from("/proc/4242/root"));
        assert_eq!(
            control.path(),
            PathBuf::from("/proc/4242/root").join(in_sandbox.strip_prefix("/").expect("relative"))
        );
        assert!(
            control
                .path()
                .starts_with("/proc/4242/root/run/jg/services/demo")
        );
    }

    fn temp_control_dir(root: &Path) -> ControlDir {
        ControlDir {
            root: root.to_path_buf(),
            relative: PathBuf::from("run/jg/services/demo/token"),
        }
    }

    fn plant_session_dir(root: &Path) -> PathBuf {
        let dir = root.join("run/jg/services/demo/token");
        std::fs::create_dir_all(&dir).expect("session dir");
        dir
    }

    fn make_fifo(path: &Path) {
        let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("fifo path");
        // SAFETY: `mkfifo` only reads the NUL-terminated path it is given.
        let result = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
        assert_eq!(result, 0, "mkfifo: {}", std::io::Error::last_os_error());
    }

    #[test]
    fn planted_stop_symlink_is_replaced_without_touching_its_target() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = plant_session_dir(temp.path());
        let target = temp.path().join("host-file");
        std::fs::write(&target, b"precious").expect("target");
        std::os::unix::fs::symlink(&target, dir.join(STOP_MARKER)).expect("stop symlink");

        let control = temp_control_dir(temp.path());
        control.request_stop().expect("stop requested");

        assert_eq!(
            std::fs::read(&target).expect("target"),
            b"precious",
            "the stop marker must never follow a symlink into a host file"
        );
        let marker = std::fs::symlink_metadata(dir.join(STOP_MARKER)).expect("stop marker");
        assert!(
            marker.file_type().is_file(),
            "the planted symlink must be replaced by a real file"
        );
    }

    #[test]
    fn planted_ready_symlink_is_not_trusted() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = plant_session_dir(temp.path());
        let target = temp.path().join("host-file");
        std::fs::write(&target, b"precious").expect("target");
        std::os::unix::fs::symlink(&target, dir.join(ready_name(ServiceKind::Postgres)))
            .expect("ready symlink");

        let control = temp_control_dir(temp.path());
        assert!(
            !control.is_ready(ServiceKind::Postgres),
            "a symlink must not make a service look ready"
        );
        assert_eq!(std::fs::read(&target).expect("target"), b"precious");
    }

    #[test]
    fn fifo_ready_marker_fails_closed_without_blocking() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = plant_session_dir(temp.path());
        make_fifo(&dir.join(ready_name(ServiceKind::Postgres)));

        let control = temp_control_dir(temp.path());
        let (sender, receiver) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(control.is_ready(ServiceKind::Postgres));
        });
        let ready = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("the ready check must not block on a planted FIFO");
        assert!(!ready, "a FIFO must not make a service look ready");
    }

    #[test]
    fn symlinked_intermediate_directory_is_not_followed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("jg/services/demo/token")).expect("target dir");
        let target = elsewhere.join("jg/services/demo/token");
        std::fs::write(target.join(ready_name(ServiceKind::Postgres)), b"").expect("ready");
        std::os::unix::fs::symlink(&elsewhere, temp.path().join("run")).expect("run symlink");

        let control = temp_control_dir(temp.path());
        assert!(
            !control.is_ready(ServiceKind::Postgres),
            "a symlinked intermediate directory must not be trusted"
        );
        assert!(
            control.request_stop().is_err(),
            "a symlinked intermediate directory must fail closed"
        );
        assert!(
            !target.join(STOP_MARKER).exists(),
            "the host must not create anything through the symlink"
        );
        control.cleanup(&[ServiceKind::Postgres]);
        assert!(
            target.join(ready_name(ServiceKind::Postgres)).exists(),
            "cleanup must not remove files through the symlink"
        );
    }

    #[test]
    fn script_prepares_starts_and_waits_for_each_service() {
        let specs = [ServiceSpec::postgres(), ServiceSpec::redis()];
        let script = build(&specs, &both_binaries());

        assert!(script.contains("set -eu"));
        assert!(script.contains("/usr/lib/postgresql/16/bin/initdb"));
        assert!(script.contains("--auth-local=trust"));
        assert!(script.contains("/usr/lib/postgresql/16/bin/pg_ctl"));
        assert!(script.contains("/usr/bin/redis-server"));
        assert!(script.contains(".s.PGSQL.5432"));
        assert!(script.contains("redis.sock"));
        assert!(script.contains("/postgres.ready'"));
        assert!(script.contains("/redis.ready'"));
        assert!(script.contains("/stop'"), "{script}");
        assert!(script.contains("sleep 0.1"), "readiness polls");
        assert!(script.contains("status >/dev/null"), "postgres liveness");
        assert!(script.contains("kill -0"), "redis liveness");
        assert!(script.contains("-m immediate -W stop"), "postgres stop");
        assert!(script.contains("exit 0"));
    }

    #[test]
    fn script_quotes_paths_with_spaces_and_quotes() {
        let quoted = shell_quote("/tmp/a b/it's");
        assert_eq!(quoted, "'/tmp/a b/it'\\''s'");
        assert_eq!(
            command_line(&["echo".to_string(), "two words".to_string()]),
            "'echo' 'two words'"
        );
    }

    #[test]
    fn readiness_timeout_is_bounded_and_counted_in_poll_ticks() {
        assert_eq!(ticks(Duration::from_secs(30), 100), 300);
        assert_eq!(ticks(Duration::from_millis(50), 100), 1);
        assert_eq!(ticks(DEFAULT_SERVICE_LIFETIME, 200), 9000);
        let script = build(&[ServiceSpec::redis()], &redis_binaries());
        assert!(script.contains("-lt 300"), "{script}");
    }

    #[test]
    fn duplicate_and_empty_services_are_rejected_before_any_exec() {
        let backend = NamespaceBackend::new();
        let specs = [ServiceSpec::postgres(), ServiceSpec::postgres()];
        let error = start_services_with(
            &backend,
            &handle(),
            &scope("demo"),
            &specs,
            &both_binaries(),
        )
        .expect_err("duplicate service");
        assert!(error.to_string().contains("more than once"), "{error}");

        let error = start_services_with(&backend, &handle(), &scope("demo"), &[], &both_binaries())
            .expect_err("no services");
        assert!(error.to_string().contains("at least one"), "{error}");
    }

    #[test]
    fn missing_binaries_fail_closed_before_any_exec() {
        let backend = NamespaceBackend::new();
        let specs = [ServiceSpec::postgres()];
        let error = start_services_with(
            &backend,
            &handle(),
            &scope("demo"),
            &specs,
            &ServiceBinaries::default(),
        )
        .expect_err("missing binaries");
        let message = error.to_string();
        for name in ["initdb", "pg_ctl", "postgres"] {
            assert!(message.contains(name), "{message} must name {name}");
        }
    }

    #[test]
    fn invalid_scope_is_rejected_before_any_exec() {
        let backend = NamespaceBackend::new();
        let specs = [ServiceSpec::redis()];
        let error = start_services_with(
            &backend,
            &handle(),
            &ExecScope::new(ScopeAccess::ReadWrite, "/etc"),
            &specs,
            &redis_binaries(),
        )
        .expect_err("invalid scope");
        assert!(error.to_string().contains("/lab"), "{error}");
    }
}

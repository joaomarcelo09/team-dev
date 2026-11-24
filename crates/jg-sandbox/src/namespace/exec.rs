//! Entering a sandbox and running one command inside it.

use std::ffi::CString;
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sched::{CloneFlags, setns, unshare};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{
    ForkResult, Pid, chdir, dup2_stderr, dup2_stdin, dup2_stdout, execve, getpid, getppid, setsid,
};

use crate::error::{Result, SandboxError};
use crate::exec::{ExecOutput, ExecRequest};
use crate::namespace::scope;
use crate::namespace::util::{self, NamespaceFds};

const START_TIMEOUT: Duration = Duration::from_secs(10);
const KILL_GRACE: Duration = Duration::from_secs(5);
/// Extra time the host waits past the request timeout before its own backstop
/// kill, so the in-sandbox supervisor can finish tearing the tree down first.
const BACKSTOP_GRACE: Duration = Duration::from_secs(10);
/// How long the output readers keep draining after the command finished.
const OUTPUT_GRACE: Duration = Duration::from_secs(1);
const READ_POLL_INTERVAL: Duration = Duration::from_millis(20);
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(5);
const READ_CHUNK: usize = 8192;

struct Pipes {
    stdin_r: OwnedFd,
    stdin_w: OwnedFd,
    stdout_r: OwnedFd,
    stdout_w: OwnedFd,
    stderr_r: OwnedFd,
    stderr_w: OwnedFd,
    status_r: OwnedFd,
    status_w: OwnedFd,
    result_r: OwnedFd,
    result_w: OwnedFd,
    pid_r: OwnedFd,
    pid_w: OwnedFd,
}

impl Pipes {
    fn new() -> Result<Self> {
        let (stdin_r, stdin_w) = util::pipe()?;
        let (stdout_r, stdout_w) = util::pipe()?;
        let (stderr_r, stderr_w) = util::pipe()?;
        let (status_r, status_w) = util::pipe()?;
        let (result_r, result_w) = util::pipe()?;
        let (pid_r, pid_w) = util::pipe()?;
        Ok(Self {
            stdin_r,
            stdin_w,
            stdout_r,
            stdout_w,
            stderr_r,
            stderr_w,
            status_r,
            status_w,
            result_r,
            result_w,
            pid_r,
            pid_w,
        })
    }
}

/// Run `request` inside the sandbox whose init is `init_pid`.
///
/// When `lab` is set, the exec worker joins the matching cgroup before doing any
/// work so the whole command tree is accounted to the lab limits: the lab's
/// init child for a lab-wide exec, or the project cgroup for a scoped exec with
/// a project. A project or project limit on a lab without a cgroup is an error
/// before anything runs. A scope's private `/tmp` cap may not exceed
/// `lab_tmp_bytes`, the sandbox's `/tmp` cap.
pub(crate) fn run(
    init_pid: i32,
    lab: Option<&Path>,
    lab_tmp_bytes: u64,
    egress_port: Option<u16>,
    hub_socket: Option<&Path>,
    request: &ExecRequest,
) -> Result<ExecOutput> {
    validate_request(request, lab_tmp_bytes)?;
    let cgroup = resolve_cgroup(lab, request)?;
    let namespaces = NamespaceFds::open(init_pid)?;
    let pipes = Pipes::new()?;

    let fork_result = util::fork_process()?;
    match fork_result {
        ForkResult::Parent { child } => run_parent(child, request, pipes),
        ForkResult::Child => child_exec(
            request,
            namespaces,
            pipes,
            cgroup.as_deref(),
            egress_port,
            hub_socket,
        ),
    }
}

/// Pick the cgroup the exec worker must join, creating and limiting a project
/// cgroup when the request asks for one. Never falls back to an unlimited run.
fn resolve_cgroup(lab: Option<&Path>, request: &ExecRequest) -> Result<Option<PathBuf>> {
    let project = request
        .scope
        .as_ref()
        .and_then(|scope| scope.project.as_deref());
    let limits = request.scope.as_ref().and_then(|scope| scope.limits);

    match (project, limits) {
        (Some(project), limits) => {
            let Some(lab) = lab else {
                return Err(SandboxError::ExecFailed {
                    reason: format!(
                        "project {project:?} needs a lab cgroup, but the sandbox has none"
                    ),
                });
            };
            let limits = limits.unwrap_or_default();
            crate::cgroup::ensure_project(lab, project, &limits).map(Some)
        }
        (None, Some(_)) => Err(SandboxError::ExecFailed {
            reason: "project limits require a project name".to_string(),
        }),
        (None, None) => Ok(lab.map(crate::cgroup::member_path)),
    }
}

fn validate_request(request: &ExecRequest, lab_tmp_bytes: u64) -> Result<()> {
    if request.argv.is_empty() || request.argv[0].is_empty() {
        return Err(SandboxError::ExecFailed {
            reason: "argv must not be empty".to_string(),
        });
    }
    if let Some(scope) = &request.scope {
        scope.validate()?;
        scope.validate_tmp_within_lab(lab_tmp_bytes)?;
    }
    if !request.cwd.is_absolute() {
        return Err(SandboxError::ExecFailed {
            reason: format!("cwd {} is not absolute", request.cwd.display()),
        });
    }
    if request
        .cwd
        .components()
        .any(|part| part == std::path::Component::ParentDir)
    {
        return Err(SandboxError::ExecFailed {
            reason: format!("cwd {} contains ..", request.cwd.display()),
        });
    }
    Ok(())
}

fn run_parent(child: Pid, request: &ExecRequest, pipes: Pipes) -> Result<ExecOutput> {
    drop(pipes.stdin_r);
    drop(pipes.stdout_w);
    drop(pipes.stderr_w);
    drop(pipes.status_w);
    drop(pipes.result_w);
    drop(pipes.pid_w);

    let gpid = match util::read_line(pipes.pid_r.as_fd(), START_TIMEOUT)? {
        Some(line) => line
            .trim()
            .parse::<i32>()
            .map_err(|_| SandboxError::ExecFailed {
                reason: format!("invalid worker pid {line:?}"),
            })?,
        None => {
            let _ = waitpid(child, None);
            return Err(SandboxError::ExecFailed {
                reason: "worker did not report a pid".to_string(),
            });
        }
    };
    drop(pipes.pid_r);

    let stdin_thread = match request.stdin.clone() {
        Some(data) => Some(spawn_stdin_writer(pipes.stdin_w, data)),
        None => {
            drop(pipes.stdin_w);
            None
        }
    };

    let finished = Arc::new(AtomicBool::new(false));
    let stdout_thread = spawn_reader(pipes.stdout_r, Arc::clone(&finished));
    let stderr_thread = spawn_reader(pipes.stderr_r, Arc::clone(&finished));

    let (_, backstop_timeout) = wait_for_worker(child, gpid, request.timeout);
    // The supervisor already killed every process of the exec on timeout; this
    // only releases the readers if a descendant still holds a pipe open.
    finished.store(true, Ordering::Relaxed);
    let stdout = join_reader(stdout_thread)?;
    let stderr = join_reader(stderr_thread)?;
    if let Some(thread) = stdin_thread {
        let _ = thread.join();
    }

    let mut status_text = String::new();
    File::from(pipes.status_r)
        .read_to_string(&mut status_text)
        .map_err(|source| SandboxError::io("<status pipe>", source))?;
    if let Some(reason) = status_text.trim().strip_prefix("ERR ") {
        return Err(SandboxError::ExecFailed {
            reason: reason.to_string(),
        });
    }

    let mut result_text = String::new();
    File::from(pipes.result_r)
        .read_to_string(&mut result_text)
        .map_err(|source| SandboxError::io("<result pipe>", source))?;

    let (exit_code, signal, worker_timeout) = parse_result(&result_text, backstop_timeout)?;
    Ok(ExecOutput {
        exit_code,
        signal,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        timed_out: backstop_timeout || worker_timeout,
        truncated: stdout.truncated || stderr.truncated,
    })
}

/// Wait for the host-side worker. The in-sandbox supervisor enforces the
/// request timeout; this wait is only a backstop so a wedged worker cannot
/// block the caller forever.
fn wait_for_worker(child: Pid, gpid: i32, timeout: Option<Duration>) -> (Option<WaitStatus>, bool) {
    let start = Instant::now();
    let limit = timeout.map(|limit| limit + BACKSTOP_GRACE);
    loop {
        match waitpid(child, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => {}
            Ok(status) => return (Some(status), false),
            Err(_) => return (None, false),
        }
        if let Some(limit) = limit
            && start.elapsed() >= limit
        {
            let _ = kill(Pid::from_raw(-gpid), Signal::SIGKILL);
            let _ = kill(child, Signal::SIGKILL);
            let _ = waitpid(child, None);
            let _ = util::wait_dead(gpid, KILL_GRACE);
            return (None, true);
        }
        std::thread::sleep(WAIT_POLL_INTERVAL);
    }
}

/// Decode the result line written by the in-sandbox init.
///
/// A missing line is a failure unless the host backstop fired before the
/// supervisor could write a result. A malformed line is always a failure.
fn parse_result(text: &str, backstop_timeout: bool) -> Result<(Option<i32>, Option<i32>, bool)> {
    let timed_out = text.lines().any(|line| line.trim() == "TIMEOUT");
    let Some(line) = text.lines().map(str::trim).find(|line| !line.is_empty()) else {
        if backstop_timeout {
            return Ok((None, None, true));
        }
        return Err(SandboxError::ExecFailed {
            reason: "the exec did not report a result".to_string(),
        });
    };
    if let Some(code) = line.strip_prefix("EXIT ")
        && let Ok(code) = code.parse::<i32>()
    {
        return Ok((Some(code), None, timed_out));
    }
    if let Some(signal) = line.strip_prefix("SIGNAL ")
        && let Ok(signal) = signal.parse::<i32>()
    {
        return Ok((None, Some(signal), timed_out));
    }
    Err(SandboxError::ExecFailed {
        reason: format!("invalid exec result {line:?}"),
    })
}

fn spawn_stdin_writer(fd: OwnedFd, data: Vec<u8>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut file = File::from(fd);
        use std::io::Write;
        let _ = file.write_all(&data);
    })
}

struct ReaderOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

/// Drain one output pipe without blocking forever.
///
/// Reading normally continues until EOF. Once `finished` is set (the command
/// exited or timed out) a leftover descendant holding the pipe open may delay
/// EOF by at most [`OUTPUT_GRACE`]; after that the reader stops and marks the
/// output truncated.
fn spawn_reader(
    fd: OwnedFd,
    finished: Arc<AtomicBool>,
) -> std::thread::JoinHandle<std::io::Result<ReaderOutput>> {
    std::thread::spawn(move || {
        let mut file = File::from(fd);
        let mut output = Vec::new();
        let mut deadline: Option<Instant> = None;
        loop {
            if deadline.is_none() && finished.load(Ordering::Relaxed) {
                deadline = Some(Instant::now() + OUTPUT_GRACE);
            }
            if let Some(limit) = deadline
                && Instant::now() >= limit
            {
                return Ok(ReaderOutput {
                    bytes: output,
                    truncated: true,
                });
            }
            let wait = match deadline {
                Some(limit) => limit.saturating_duration_since(Instant::now()),
                None => READ_POLL_INTERVAL,
            };
            let timeout = PollTimeout::try_from(wait).unwrap_or(PollTimeout::MAX);
            let mut fds = [PollFd::new(file.as_fd(), PollFlags::POLLIN)];
            match poll(&mut fds, timeout) {
                Ok(0) => continue,
                Ok(_) => {}
                Err(nix::errno::Errno::EINTR) => continue,
                Err(source) => {
                    return Err(std::io::Error::from_raw_os_error(source as i32));
                }
            }
            let mut buffer = [0u8; READ_CHUNK];
            match file.read(&mut buffer) {
                Ok(0) => {
                    return Ok(ReaderOutput {
                        bytes: output,
                        truncated: false,
                    });
                }
                Ok(count) => output.extend_from_slice(&buffer[..count]),
                Err(source) if source.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(source) => return Err(source),
            }
        }
    })
}

fn join_reader(
    thread: std::thread::JoinHandle<std::io::Result<ReaderOutput>>,
) -> Result<ReaderOutput> {
    match thread.join() {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(source)) => Err(SandboxError::io("<output pipe>", source)),
        Err(_) => Err(SandboxError::ExecFailed {
            reason: "output reader thread panicked".to_string(),
        }),
    }
}

fn child_exec(
    request: &ExecRequest,
    namespaces: NamespaceFds,
    pipes: Pipes,
    cgroup: Option<&Path>,
    egress_port: Option<u16>,
    hub_socket: Option<&Path>,
) -> ! {
    let Pipes {
        stdin_r,
        stdin_w,
        stdout_r,
        stdout_w,
        stderr_r,
        stderr_w,
        status_r,
        status_w,
        result_r,
        result_w,
        pid_r,
        pid_w,
    } = pipes;
    drop(stdin_w);
    drop(stdout_r);
    drop(stderr_r);
    drop(status_r);
    drop(result_r);
    drop(pid_r);

    // Join the resolved cgroup (the lab init child or a project cgroup) while
    // still in the host namespace, so the sandbox user namespace switch below
    // cannot change the accounting target.
    if let Some(path) = cgroup
        && let Err(error) = crate::cgroup::add_pid(path, getpid().as_raw())
    {
        report_worker_error(&status_w, &error);
        util::exit_now(127)
    }

    let NamespaceFds {
        user,
        mount,
        pid,
        ipc,
        uts,
        net,
    } = namespaces;

    let joined = setns(user.as_fd(), CloneFlags::CLONE_NEWUSER)
        .map_err(|source| SandboxError::nix("join user namespace", source))
        .and_then(|()| {
            setns(pid.as_fd(), CloneFlags::CLONE_NEWPID)
                .map_err(|source| SandboxError::nix("join pid namespace", source))
        });
    if let Err(error) = joined {
        report_worker_error(&status_w, &error);
        util::exit_now(127)
    }

    let fork_result = util::fork_process();
    let fork_result = match fork_result {
        Ok(result) => result,
        Err(error) => {
            report_worker_error(&status_w, &error);
            util::exit_now(127)
        }
    };

    match fork_result {
        ForkResult::Child => {
            drop(user);
            drop(pid);
            drop(pid_w);
            supervisor_exec(
                request,
                mount,
                ipc,
                uts,
                net,
                stdin_r,
                stdout_w,
                stderr_w,
                status_w,
                result_w,
                egress_port,
                hub_socket,
            )
        }
        ForkResult::Parent { child } => {
            drop(mount);
            drop(ipc);
            drop(uts);
            drop(net);
            drop(user);
            drop(pid);
            drop(stdin_r);
            drop(stdout_w);
            drop(stderr_w);
            drop(status_w);
            drop(result_w);

            let _ = util::write_all(pid_w.as_fd(), format!("{}\n", child.as_raw()).as_bytes());
            drop(pid_w);

            let _ = waitpid(child, None);
            util::exit_now(0)
        }
    }
}

/// Supervisor for one exec, running inside the sandbox PID namespace.
///
/// It creates a nested PID namespace for the exec and forks its init there.
/// The command is a child of that init, so it cannot see or signal this
/// supervisor, and `SIGKILL`/`SIGSTOP` aimed at its parent only reach the
/// nested init, which ignores signals from its own namespace. The supervisor
/// enforces the request timeout by killing that init; the kernel then kills
/// every process left in the nested namespace, including `setsid` and
/// double-forked daemons.
#[allow(clippy::too_many_arguments)]
fn supervisor_exec(
    request: &ExecRequest,
    mount: OwnedFd,
    ipc: OwnedFd,
    uts: OwnedFd,
    net: OwnedFd,
    stdin_r: OwnedFd,
    stdout_w: OwnedFd,
    stderr_w: OwnedFd,
    status_w: OwnedFd,
    result_w: OwnedFd,
    egress_port: Option<u16>,
    hub_socket: Option<&Path>,
) -> ! {
    // Join the sandbox filesystem view before creating the nested PID
    // namespace, so the whole exec shares the sandbox mounts and devices.
    if let Err(error) = join_sandbox(&mount, &ipc, &uts, &net) {
        report_worker_error(&status_w, &error);
        util::exit_now(127)
    }
    // A session of its own lets the host backstop kill the whole supervisor
    // group instead of reaching the caller's process group.
    if let Err(error) = setsid() {
        report_worker_error(&status_w, &SandboxError::nix("setsid supervisor", error));
        util::exit_now(127)
    }
    if let Err(error) = util::set_child_subreaper() {
        report_worker_error(&status_w, &error);
        util::exit_now(127)
    }
    // From now on the next child becomes PID 1 of a private PID namespace.
    // The supervisor itself stays in the sandbox namespace, where only host
    // ancestors and it can reach the nested processes.
    if let Err(error) = unshare(CloneFlags::CLONE_NEWPID)
        .map_err(|source| SandboxError::nix("unshare exec pid namespace", source))
    {
        report_worker_error(&status_w, &error);
        util::exit_now(127)
    }

    let nested_init_parent = getpid();
    let fork_result = util::fork_process();
    let fork_result = match fork_result {
        Ok(result) => result,
        Err(error) => {
            report_worker_error(&status_w, &error);
            util::exit_now(127)
        }
    };

    match fork_result {
        ForkResult::Child => nested_init_exec(
            request,
            mount,
            ipc,
            uts,
            net,
            stdin_r,
            stdout_w,
            stderr_w,
            status_w,
            result_w,
            nested_init_parent,
            egress_port,
            hub_socket,
        ),
        ForkResult::Parent { child } => {
            drop(mount);
            drop(ipc);
            drop(uts);
            drop(net);
            drop(stdin_r);
            drop(stdout_w);
            drop(stderr_w);
            drop(status_w);

            // The nested init writes the command's result itself. Only a
            // timeout needs a line from here, after the init was killed.
            if let Some(line) = supervise(child, request.timeout) {
                let _ = util::write_all(result_w.as_fd(), line.as_bytes());
                let _ = util::write_all(result_w.as_fd(), b"TIMEOUT\n");
            }
            kill_remaining_children();
            drop(result_w);
            util::exit_now(0)
        }
    }
}

/// Wait for the nested init and enforce `timeout`.
///
/// Returns the result line to append only when the timeout killed the init; on
/// a normal exit the init has already written the command's result.
fn supervise(init: Pid, timeout: Option<Duration>) -> Option<String> {
    let start = Instant::now();
    loop {
        match waitpid(init, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => {}
            Ok(_) => return None,
            Err(_) => return None,
        }
        if let Some(limit) = timeout
            && start.elapsed() >= limit
        {
            // Killing the nested init makes the kernel kill every process of
            // its PID namespace, including detached descendants.
            let _ = kill(init, Signal::SIGKILL);
            let line = match waitpid(init, None) {
                Ok(status) => result_line(status),
                Err(_) => "SIGNAL 9\n".to_string(),
            };
            return Some(line);
        }
        std::thread::sleep(WAIT_POLL_INTERVAL);
    }
}

/// PID 1 of one exec's private PID namespace.
///
/// It forks the command and reports the command's status over `result_w`. The
/// command closes that write end before `exec`, so it cannot forge a result.
/// When this init exits the kernel kills every process still in the namespace,
/// which is what tears down `setsid`/double-forked descendants.
#[allow(clippy::too_many_arguments)]
fn nested_init_exec(
    request: &ExecRequest,
    mount: OwnedFd,
    ipc: OwnedFd,
    uts: OwnedFd,
    net: OwnedFd,
    stdin_r: OwnedFd,
    stdout_w: OwnedFd,
    stderr_w: OwnedFd,
    status_w: OwnedFd,
    result_w: OwnedFd,
    expected_parent: Pid,
    egress_port: Option<u16>,
    hub_socket: Option<&Path>,
) -> ! {
    // If the supervisor dies (for example the host backstop kills it), this
    // init must die too, so the kernel tears the nested namespace down.
    if let Err(error) = arm_parent_death_signal(expected_parent, false) {
        report_worker_error(&status_w, &error);
        let _ = util::write_all(result_w.as_fd(), b"EXIT 255\n");
        util::exit_now(127)
    }

    if let Err(error) = sanitize_nested_init_identity() {
        report_worker_error(&status_w, &error);
        let _ = util::write_all(result_w.as_fd(), b"EXIT 255\n");
        util::exit_now(127)
    }

    let worker_parent = getpid();
    let fork_result = util::fork_process();
    let fork_result = match fork_result {
        Ok(result) => result,
        Err(error) => {
            report_worker_error(&status_w, &error);
            let _ = util::write_all(result_w.as_fd(), b"EXIT 255\n");
            util::exit_now(127)
        }
    };

    match fork_result {
        ForkResult::Child => {
            drop(result_w);
            worker_exec(
                request,
                mount,
                ipc,
                uts,
                net,
                stdin_r,
                stdout_w,
                stderr_w,
                status_w,
                worker_parent,
                egress_port,
                hub_socket,
            )
        }
        ForkResult::Parent { child } => {
            drop(mount);
            drop(ipc);
            drop(uts);
            drop(net);
            drop(stdin_r);
            drop(stdout_w);
            drop(stderr_w);
            drop(status_w);

            let line = match waitpid(child, None) {
                Ok(status) => result_line(status),
                Err(_) => "EXIT 255\n".to_string(),
            };
            let _ = util::write_all(result_w.as_fd(), line.as_bytes());
            drop(result_w);
            // Returning here exits PID 1; the kernel then kills every process
            // left in the nested namespace.
            util::exit_now(0)
        }
    }
}

fn result_line(status: WaitStatus) -> String {
    match status {
        WaitStatus::Exited(_, code) => format!("EXIT {code}\n"),
        WaitStatus::Signaled(_, signal, _) => format!("SIGNAL {}\n", signal as i32),
        _ => "EXIT 255\n".to_string(),
    }
}

fn sanitize_nested_init_identity() -> Result<()> {
    const INIT_NAME: &[u8] = b"jg-sandbox-init\0";
    // SAFETY: `prctl` receives a valid NUL-terminated name and scalar arguments.
    let result = unsafe {
        libc::prctl(
            libc::PR_SET_NAME,
            INIT_NAME.as_ptr() as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    if result != 0 {
        return Err(SandboxError::nix(
            "set nested init process name",
            nix::errno::Errno::last(),
        ));
    }

    clear_nested_init_arguments_and_environment()
}

fn clear_nested_init_arguments_and_environment() -> Result<()> {
    let stat = std::fs::read_to_string("/proc/self/stat").map_err(|source| SandboxError::Io {
        path: PathBuf::from("/proc/self/stat"),
        source,
    })?;
    let fields = stat
        .rsplit_once(')')
        .map(|(_, fields)| fields.split_whitespace().collect::<Vec<_>>())
        .ok_or_else(|| SandboxError::ExecFailed {
            reason: "could not parse /proc/self/stat".to_string(),
        })?;
    // /proc/<pid>/stat starts these fields at field 3 (state).
    let arg_start = parse_proc_stat_address(&fields, 45, "argument start")?;
    let arg_end = parse_proc_stat_address(&fields, 46, "argument end")?;
    let env_start = parse_proc_stat_address(&fields, 47, "environment start")?;
    let env_end = parse_proc_stat_address(&fields, 48, "environment end")?;

    clear_process_memory_range(arg_start, arg_end)?;
    clear_process_memory_range(env_start, env_end)
}

fn parse_proc_stat_address(fields: &[&str], index: usize, name: &str) -> Result<usize> {
    fields
        .get(index)
        .and_then(|address| address.parse().ok())
        .ok_or_else(|| SandboxError::ExecFailed {
            reason: format!("could not read {name} from /proc/self/stat"),
        })
}

fn clear_process_memory_range(start: usize, end: usize) -> Result<()> {
    if start > end {
        return Err(SandboxError::ExecFailed {
            reason: "invalid argument or environment range in /proc/self/stat".to_string(),
        });
    }
    if start < end {
        // SAFETY: The kernel reports these writable argument/environment ranges
        // for this process in /proc/self/stat. This process is the forked init,
        // so clearing them does not modify the supervisor's copy.
        unsafe {
            std::ptr::write_bytes(start as *mut u8, 0, end - start);
        }
    }
    Ok(())
}

/// Kill any direct child left after the nested init was reaped.
///
/// The kernel already kills the whole nested namespace when its init dies; this
/// is a backstop for the supervisor's own helper processes.
fn kill_remaining_children() {
    let me = getpid().as_raw();
    let deadline = Instant::now() + KILL_GRACE;
    loop {
        reap_children();
        let children = live_children(me);
        if children.is_empty() {
            return;
        }
        for pid in children {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
        if Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(WAIT_POLL_INTERVAL);
    }
}

fn reap_children() {
    while let Ok(status) = waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
        if status == WaitStatus::StillAlive {
            return;
        }
    }
}

/// Live (non-zombie) children of `parent`, as seen in the sandbox PID namespace.
fn live_children(parent: i32) -> Vec<i32> {
    let mut children = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return children;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Some((state, ppid)) = proc_state_and_parent(pid) else {
            continue;
        };
        if ppid == parent && !matches!(state, 'Z' | 'X' | 'x') {
            children.push(pid);
        }
    }
    children
}

fn proc_state_and_parent(pid: i32) -> Option<(char, i32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse().ok()?;
    Some((state, ppid))
}

fn arm_parent_death_signal(expected_parent: Pid, parent_is_visible: bool) -> Result<()> {
    util::die_with_parent()?;

    // getppid() returns zero when nested_init's parent is outside its PID
    // namespace. Its procfs is still the sandbox-wide mount at this point, so
    // also ensure the known parent is alive there.
    let expected_ppid = if parent_is_visible {
        expected_parent
    } else {
        Pid::from_raw(0)
    };
    let actual_ppid = getppid();
    let parent_is_alive = parent_is_visible
        || proc_state_and_parent(expected_parent.as_raw())
            .is_some_and(|(state, _)| !matches!(state, 'Z' | 'X' | 'x'));
    if actual_ppid != expected_ppid || !parent_is_alive {
        return Err(SandboxError::ExecFailed {
            reason: format!(
                "parent changed while setting parent death signal (expected {expected_parent}, observed {actual_ppid})"
            ),
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn worker_exec(
    request: &ExecRequest,
    mount: OwnedFd,
    ipc: OwnedFd,
    uts: OwnedFd,
    net: OwnedFd,
    stdin_r: OwnedFd,
    stdout_w: OwnedFd,
    stderr_w: OwnedFd,
    status_w: OwnedFd,
    expected_parent: Pid,
    egress_port: Option<u16>,
    hub_socket: Option<&Path>,
) -> ! {
    match worker_setup(
        request,
        &mount,
        &ipc,
        &uts,
        &net,
        &stdin_r,
        &stdout_w,
        &stderr_w,
        &status_w,
        expected_parent,
        egress_port,
        hub_socket,
    ) {
        Ok(()) => util::exit_now(127),
        Err(error) => {
            report_worker_error(&status_w, &error);
            util::exit_now(127)
        }
    }
}

fn report_worker_error(fd: &OwnedFd, error: &SandboxError) {
    let _ = util::write_all(fd.as_fd(), format!("ERR {error}\n").as_bytes());
}

fn join_sandbox(mount: &OwnedFd, ipc: &OwnedFd, uts: &OwnedFd, net: &OwnedFd) -> Result<()> {
    setns(mount.as_fd(), CloneFlags::CLONE_NEWNS)
        .map_err(|source| SandboxError::nix("join mount namespace", source))?;
    setns(ipc.as_fd(), CloneFlags::CLONE_NEWIPC)
        .map_err(|source| SandboxError::nix("join ipc namespace", source))?;
    setns(uts.as_fd(), CloneFlags::CLONE_NEWUTS)
        .map_err(|source| SandboxError::nix("join uts namespace", source))?;
    setns(net.as_fd(), CloneFlags::CLONE_NEWNET)
        .map_err(|source| SandboxError::nix("join net namespace", source))
}

#[allow(clippy::too_many_arguments)]
fn worker_setup(
    request: &ExecRequest,
    mount: &OwnedFd,
    ipc: &OwnedFd,
    uts: &OwnedFd,
    net: &OwnedFd,
    stdin_r: &OwnedFd,
    stdout_w: &OwnedFd,
    stderr_w: &OwnedFd,
    status_w: &OwnedFd,
    expected_parent: Pid,
    egress_port: Option<u16>,
    hub_socket: Option<&Path>,
) -> Result<()> {
    arm_parent_death_signal(expected_parent, true)?;
    join_sandbox(mount, ipc, uts, net)?;
    if let Some(scope) = &request.scope {
        scope::enter_scope(scope)?;
        if !request.env.contains_key("HOME") {
            scope::create_scoped_home()?;
        }
    } else {
        // Unscoped execs need the same PID-scoped procfs as scoped execs.
        unshare(CloneFlags::CLONE_NEWNS)
            .map_err(|source| SandboxError::nix("unshare exec mount namespace", source))?;
        util::make_mounts_private()?;
        util::mount_proc(Path::new("/proc"))?;
    }

    // A fresh session detaches the command from the caller's job control and
    // its controlling terminal; the timeout kill still reaches the group because
    // the session leader is also the group leader.
    setsid().map_err(|source| SandboxError::nix("setsid", source))?;
    dup2_stdin(stdin_r.as_fd()).map_err(|source| SandboxError::nix("redirect stdin", source))?;
    dup2_stdout(stdout_w.as_fd()).map_err(|source| SandboxError::nix("redirect stdout", source))?;
    dup2_stderr(stderr_w.as_fd()).map_err(|source| SandboxError::nix("redirect stderr", source))?;

    let cwd = request.effective_cwd();
    chdir(&cwd).map_err(|source| SandboxError::nix(format!("chdir {}", cwd.display()), source))?;

    let (program, argv, env) = prepare_exec(request, egress_port, hub_socket)?;
    util::drop_all_privileges()?;
    // Only the stdio dups and the close-on-exec status pipe may survive here.
    util::close_inherited_fds(status_w.as_fd());
    match execve(&program, &argv, &env) {
        Ok(never) => match never {},
        Err(source) => Err(SandboxError::ExecFailed {
            reason: format!("execve {}: {source}", program.to_string_lossy()),
        }),
    }
}

fn prepare_exec(
    request: &ExecRequest,
    egress_port: Option<u16>,
    hub_socket: Option<&Path>,
) -> Result<(CString, Vec<CString>, Vec<CString>)> {
    let env = request.resolved_env(egress_port, hub_socket);
    let program = resolve_program(&request.argv[0], env.get("PATH").map(String::as_str))?;
    let program = c_string(&program)?;
    let argv = request
        .argv
        .iter()
        .map(|arg| c_string(arg))
        .collect::<Result<Vec<_>>>()?;
    let env = env
        .iter()
        .map(|(key, value)| c_string(&format!("{key}={value}")))
        .collect::<Result<Vec<_>>>()?;
    Ok((program, argv, env))
}

fn resolve_program(program: &str, path: Option<&str>) -> Result<String> {
    if program.contains('/') {
        return Ok(program.to_string());
    }
    let path = path.unwrap_or(crate::exec::DEFAULT_PATH);
    for dir in path.split(':') {
        let dir = if dir.is_empty() { "." } else { dir };
        let candidate = format!("{dir}/{program}");
        if is_executable(Path::new(&candidate)) {
            return Ok(candidate);
        }
    }
    Err(SandboxError::ExecFailed {
        reason: format!("program {program:?} not found in PATH"),
    })
}

fn is_executable(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

fn c_string(text: &str) -> Result<CString> {
    CString::new(text).map_err(|_| SandboxError::ExecFailed {
        reason: format!("value {text:?} contains a NUL byte"),
    })
}

#[cfg(test)]
mod tests {
    use super::parse_result;

    #[test]
    fn missing_result_is_a_failure() {
        assert!(parse_result("", false).is_err());
    }

    #[test]
    fn missing_result_after_backstop_is_a_timeout() {
        assert_eq!(
            parse_result("", true).expect("backstop timeout"),
            (None, None, true)
        );
    }

    #[test]
    fn malformed_result_is_a_failure() {
        assert!(parse_result("EXIT nope\n", false).is_err());
    }

    #[test]
    fn exit_and_timeout_lines_are_decoded() {
        let (exit_code, signal, timed_out) =
            parse_result("SIGNAL 9\nTIMEOUT\n", false).expect("result");
        assert_eq!(exit_code, None);
        assert_eq!(signal, Some(9));
        assert!(timed_out);
    }
}

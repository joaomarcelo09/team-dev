//! Service lifecycle: fake PostgreSQL and Redis daemons in a real sandbox.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jg_sandbox::{
    Backend, ExecRequest, ExecScope, NamespaceBackend, SandboxError, ScopeAccess, ServiceBinaries,
    ServiceSet, ServiceSpec, start_services_with,
};
use support::{create_or_skip, fixture, is_zombie, process_args, run_ok};

fn scope() -> ExecScope {
    ExecScope::new(ScopeAccess::ReadWrite, "/lab/proj")
}

/// Write an executable script at `dir/name`.
fn write_executable(dir: &Path, name: &str, body: &str) -> PathBuf {
    std::fs::create_dir_all(dir).expect("script dir");
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write script");
    let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).expect("permissions");
    path
}

/// Fake programs at their in-sandbox paths: the worktree is `/workspace`.
fn fake_binaries() -> ServiceBinaries {
    ServiceBinaries {
        initdb: Some(PathBuf::from("/workspace/fakebin/initdb")),
        pg_ctl: Some(PathBuf::from("/workspace/fakebin/pg_ctl")),
        postgres: Some(PathBuf::from("/workspace/fakebin/postgres")),
        redis_server: Some(PathBuf::from("/workspace/fakebin/redis-server")),
    }
}

fn install_fakes(worktree: &Path, token: &str) {
    let bin = worktree.join("fakebin");
    write_executable(
        &bin,
        "initdb",
        "#!/bin/sh\nset -eu\ndata=\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    -D) data=$2; shift 2;;\n    *) shift;;\n  esac\ndone\n[ -n \"$data\" ]\nmkdir -p \"$data\"\n",
    );
    write_executable(&bin, "postgres", "#!/bin/sh\nexit 0\n");
    write_executable(
        &bin,
        "pg_ctl",
        &format!(
            "#!/bin/sh\nset -eu\ndata=\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    -D) data=$2; shift 2;;\n    -l) shift 2;;\n    -p) shift 2;;\n    -o) shift 2;;\n    -m|-t) shift 2;;\n    -w) shift;;\n    start)\n      run=\"$(dirname \"$data\")/run\"\n      mkdir -p \"$run\"\n      setsid sh -c 'while :; do sleep 1; done' {token}-pg >/dev/null 2>&1 &\n      : > \"$run/.s.PGSQL.5432\"\n      exit 0;;\n    status) exit 0;;\n    stop) exit 0;;\n    *) shift;;\n  esac\ndone\n"
        ),
    );
    write_executable(
        &bin,
        "redis-server",
        &format!(
            "#!/bin/sh\nset -eu\nsocket=\npidfile=\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    --unixsocket) socket=$2; shift 2;;\n    --pidfile) pidfile=$2; shift 2;;\n    --save|--dir|--logfile|--daemonize|--port|--unixsocketperm|--appendonly) shift 2;;\n    *) shift;;\n  esac\ndone\n[ -n \"$socket\" ]\n[ -n \"$pidfile\" ]\nsetsid sh -c 'echo $$ > \"$1\"; : > \"$2\"; while :; do sleep 1; done' sh \"$pidfile\" \"$socket\" {token}-redis >/dev/null 2>&1 &\n"
        ),
    );
}

/// Whether a live process of the host has `needle` in its command line.
fn process_with_arg(needle: &str) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        if !process_args(pid).iter().any(|arg| arg.contains(needle)) {
            continue;
        }
        if !is_zombie(pid) {
            return true;
        }
    }
    false
}

fn wait_until_gone(needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_with_arg(needle) {
        assert!(
            Instant::now() < deadline,
            "{needle} is still alive after the session was stopped"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Stop `set` on another thread and fail unless it returns within `limit`.
///
/// A stop that waits for the session backstop would block this for about half
/// an hour, so the limit turns that regression into a prompt test failure.
fn stop_within(set: ServiceSet, limit: Duration) -> std::result::Result<(), SandboxError> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(set.stop());
    });
    receiver
        .recv_timeout(limit)
        .expect("stop must not wait out the service lifetime")
}

#[test]
fn fake_services_start_share_their_env_and_stop() {
    let lab = fixture();
    let worktree = lab.spec.lab_dir.join("proj");
    std::fs::create_dir_all(&worktree).expect("worktree");
    let token = format!("jg-services-{}", std::process::id());
    install_fakes(&worktree, &token);
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };

    let specs = [ServiceSpec::postgres(), ServiceSpec::redis()];
    let set = start_services_with(&backend, guard.handle(), &scope(), &specs, &fake_binaries())
        .expect("services start");

    assert_eq!(set.project(), "proj");
    assert_eq!(set.paths().len(), 2);
    let env = set.env();
    assert_eq!(env["PGHOST"], "/tmp/jg-services/proj/postgres/run");
    assert_eq!(
        env["DATABASE_URL"],
        "postgresql://postgres@/postgres?host=/tmp/jg-services/proj/postgres/run"
    );
    assert_eq!(
        env["REDIS_URL"],
        "unix:///tmp/jg-services/proj/redis/run/redis.sock"
    );

    assert!(
        process_with_arg(&format!("{token}-pg")),
        "the PostgreSQL fake must be running"
    );
    assert!(
        process_with_arg(&format!("{token}-redis")),
        "the Redis fake must be running"
    );

    set.stop().expect("stop");
    wait_until_gone(&format!("{token}-pg"));
    wait_until_gone(&format!("{token}-redis"));
}

#[test]
fn a_service_that_exits_early_fails_the_start() {
    let lab = fixture();
    let worktree = lab.spec.lab_dir.join("proj");
    std::fs::create_dir_all(&worktree).expect("worktree");
    install_fakes(&worktree, "never-started");
    write_executable(
        &worktree.join("fakebin"),
        "redis-server",
        "#!/bin/sh\nexit 1\n",
    );
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };

    let error = start_services_with(
        &backend,
        guard.handle(),
        &scope(),
        &[ServiceSpec::redis()],
        &fake_binaries(),
    )
    .expect_err("the start must fail");
    let message = error.to_string();
    assert!(
        message.contains("supervisor exited before"),
        "unexpected error: {message}"
    );
}

/// The finding behind the supervisor design: a daemon never survives its exec.
#[test]
fn a_daemon_does_not_outlive_its_exec() {
    let lab = fixture();
    std::fs::create_dir_all(lab.spec.lab_dir.join("proj")).expect("worktree");
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let token = format!("jg-daemon-probe-{}", std::process::id());
    let script = format!(
        "setsid sh -c 'while :; do sleep 1; done' {token} >/dev/null 2>&1 &\nsleep 0.2\ngrep -lq {token} /proc/[0-9]*/cmdline\n"
    );
    let request = ExecRequest::new(vec!["/bin/sh".to_string(), "-c".to_string(), script])
        .with_scope(scope())
        .with_timeout(Duration::from_secs(30));

    let output = backend.exec(guard.handle(), &request).expect("exec");
    assert_eq!(
        output.exit_code,
        Some(0),
        "the daemon must be alive inside its exec: {}",
        output.stderr_text()
    );
    wait_until_gone(&token);
}

/// A sandbox that removes the stop channel must not stall `stop` for 31 min.
#[test]
fn stop_after_the_session_dir_is_deleted_is_prompt_and_kills_the_daemons() {
    let lab = fixture();
    let worktree = lab.spec.lab_dir.join("proj");
    std::fs::create_dir_all(&worktree).expect("worktree");
    let token = format!("jg-deleted-{}", std::process::id());
    install_fakes(&worktree, &token);
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };

    let specs = [ServiceSpec::postgres(), ServiceSpec::redis()];
    let set = start_services_with(&backend, guard.handle(), &scope(), &specs, &fake_binaries())
        .expect("services start");
    assert!(process_with_arg(&format!("{token}-pg")), "postgres fake");
    assert!(process_with_arg(&format!("{token}-redis")), "redis fake");

    run_ok(backend, guard.handle(), "rm -rf /run/jg/services/proj");

    let result = stop_within(set, Duration::from_secs(10));
    assert!(
        result.is_err(),
        "a deleted stop channel must fail closed, got {result:?}"
    );
    wait_until_gone(&format!("{token}-pg"));
    wait_until_gone(&format!("{token}-redis"));
}

/// Replacing the session directory with a symlink must not stall `stop` either.
#[test]
fn stop_after_the_session_dir_is_replaced_is_prompt_and_kills_the_daemons() {
    let lab = fixture();
    let worktree = lab.spec.lab_dir.join("proj");
    std::fs::create_dir_all(&worktree).expect("worktree");
    let token = format!("jg-replaced-{}", std::process::id());
    install_fakes(&worktree, &token);
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };

    let specs = [ServiceSpec::postgres(), ServiceSpec::redis()];
    let set = start_services_with(&backend, guard.handle(), &scope(), &specs, &fake_binaries())
        .expect("services start");
    assert!(process_with_arg(&format!("{token}-pg")), "postgres fake");
    assert!(process_with_arg(&format!("{token}-redis")), "redis fake");

    run_ok(
        backend,
        guard.handle(),
        "for dir in /run/jg/services/proj/*; do rm -rf \"$dir\"; ln -s /tmp \"$dir\"; done",
    );

    let result = stop_within(set, Duration::from_secs(10));
    assert!(
        result.is_err(),
        "a replaced stop channel must fail closed, got {result:?}"
    );
    wait_until_gone(&format!("{token}-pg"));
    wait_until_gone(&format!("{token}-redis"));
}

//! Fail-closed behaviour and spec validation tests.

mod support;

use std::path::PathBuf;
use std::time::Duration;

use jg_sandbox::{
    Backend, ExecRequest, NamespaceBackend, NamespaceIds, SandboxError, SandboxSpec,
    authorize_full_allow,
};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use support::{assert_process_gone, create_or_skip, fixture, run_ok};

#[test]
fn exec_refuses_a_sandbox_that_does_not_verify() {
    let lab = fixture();
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();

    run_ok(backend, handle, "true");
    let init_pid = handle.init_pid();
    kill(Pid::from_raw(init_pid), Signal::SIGKILL).expect("kill init");
    assert_process_gone(init_pid);

    let request = ExecRequest::new(vec!["/bin/true".to_string()]);
    match backend.exec(handle, &request) {
        Err(SandboxError::NotVerified { .. }) => {}
        other => panic!("exec after init death must refuse, got {other:?}"),
    }
}

#[test]
fn full_allow_requires_a_healthy_verified_sandbox() {
    let lab = fixture();
    let backend = NamespaceBackend::new();

    assert!(
        backend.attach(&lab.spec.state_dir).is_err(),
        "attach without a sandbox must fail"
    );

    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();
    let permit = authorize_full_allow(&backend, handle).expect("healthy sandbox must authorize");
    assert_eq!(permit.state_dir(), handle.state_dir());
    assert_eq!(permit.init_pid(), handle.init_pid());
    assert_eq!(
        permit.namespaces(),
        NamespaceIds::read(handle.init_pid()).expect("read namespace ids"),
        "the permit must expose the verified namespace ids"
    );

    let state_path = lab.spec.state_dir.join("state.json");
    let original = std::fs::read_to_string(&state_path).expect("read state");
    let mut value: serde_json::Value = serde_json::from_str(&original).expect("parse state");
    let user = value["namespaces"]["user"].as_u64().expect("user ns id");
    value["namespaces"]["user"] = serde_json::Value::from(user + 1);
    std::fs::write(
        &state_path,
        serde_json::to_string_pretty(&value).expect("state json"),
    )
    .expect("write tampered state");
    assert!(
        authorize_full_allow(&backend, handle).is_err(),
        "tampered namespace ids must not authorize"
    );
    std::fs::write(&state_path, &original).expect("restore state");
    assert!(
        authorize_full_allow(&backend, handle).is_ok(),
        "restored state must authorize again"
    );

    let init_pid = handle.init_pid();
    kill(Pid::from_raw(init_pid), Signal::SIGKILL).expect("kill init");
    assert_process_gone(init_pid);
    assert!(
        authorize_full_allow(&backend, handle).is_err(),
        "dead init must not authorize"
    );

    backend.destroy(handle).expect("destroy");
    assert!(
        authorize_full_allow(&backend, handle).is_err(),
        "destroyed sandbox must not authorize"
    );
    assert!(backend.attach(&lab.spec.state_dir).is_err());
}

#[test]
fn invalid_specs_are_rejected_before_start() {
    let temp = tempfile::tempdir().expect("temp dir");
    let lab = temp.path().join("lab");
    std::fs::create_dir_all(&lab).expect("lab dir");
    let backend = NamespaceBackend::new();

    let cases: Vec<(SandboxSpec, &str)> = vec![
        (
            SandboxSpec::new(PathBuf::from("relative-state"), &lab),
            "relative state dir",
        ),
        (
            SandboxSpec::new(
                temp.path().join("state-dotdot"),
                temp.path().join("x/../lab"),
            ),
            "parent component in lab dir",
        ),
        (
            SandboxSpec::new(
                temp.path().join("state-missing-lab"),
                temp.path().join("missing"),
            ),
            "missing lab dir",
        ),
        (
            {
                let mut spec = SandboxSpec::new(temp.path().join("state-ro"), &lab);
                spec.ro_binds = vec![temp.path().join("missing-bind")];
                spec
            },
            "missing read-only bind",
        ),
        (
            {
                let mut spec = SandboxSpec::new(temp.path().join("state-socket"), &lab);
                spec.allowed_sockets = vec![temp.path().join("missing.sock")];
                spec
            },
            "missing allowed socket",
        ),
        (
            {
                let mut spec = SandboxSpec::new(temp.path().join("state-hostname"), &lab);
                spec.hostname = "bad hostname".to_string();
                spec
            },
            "invalid hostname",
        ),
    ];

    for (spec, label) in cases {
        let state_dir = spec.state_dir.clone();
        match backend.create(&spec) {
            Err(SandboxError::InvalidSpec { .. }) => {}
            other => panic!("{label} must be rejected, got {other:?}"),
        }
        assert!(
            !state_dir.exists(),
            "{label} created the state directory before validating"
        );
    }
}

#[test]
fn exec_rejects_relative_cwd_and_empty_argv() {
    let lab = fixture();
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();

    let empty = ExecRequest::new(Vec::new());
    assert!(backend.exec(handle, &empty).is_err());

    let relative = ExecRequest::new(vec!["/bin/true".to_string()]).with_cwd("relative");
    assert!(backend.exec(handle, &relative).is_err());
}

#[test]
fn destroy_tears_down_its_pid_namespace() {
    let lab = fixture();
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();
    let init_pid = handle.init_pid();
    let namespace = support::pid_namespace_inode(init_pid);

    let worker_handle = handle.clone();
    let worker = std::thread::spawn(move || {
        let request = ExecRequest::new(vec!["/bin/sleep".to_string(), "30".to_string()]);
        backend.exec(&worker_handle, &request)
    });

    wait_until("worker joins the sandbox", || {
        support::pids_in_pid_namespace(namespace).len() >= 2
    });

    backend.destroy(handle).expect("destroy");
    let _ = worker.join();
    wait_until("every sandbox process is gone", || {
        support::pids_in_pid_namespace(namespace).is_empty()
    });
    assert_process_gone(init_pid);
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting until {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

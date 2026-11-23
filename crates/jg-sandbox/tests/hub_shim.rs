//! Integration tests for the hub shim socket exposed inside a sandbox.

mod support;

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::thread;
use std::time::Duration;

use jg_sandbox::{
    Backend, DEFAULT_EGRESS_PORT, EgressSpec, ExecRequest, ExecScope, NamespaceBackend,
    SandboxError, SandboxHandle, SandboxSpec, ScopeAccess,
};
use support::{create_or_skip, fixture};

/// Marker read by the helper test when it runs inside a sandbox.
const CLIENT_MODE: &str = "JG_HUB_CLIENT_MODE";
/// Where the hub socket is exposed inside a sandbox.
const HUB_SOCKET_PATH: &str = "/run/jg/hub.sock";
/// Where an egress proxy socket is exposed inside a sandbox.
const EGRESS_SOCKET_PATH: &str = "/run/jg/egress.sock";

/// Echo the next four bytes received on `listener`, once, then return.
fn spawn_echo_server(socket_path: &Path) -> thread::JoinHandle<()> {
    let listener = UnixListener::bind(socket_path).expect("bind echo server");
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept echo connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("echo read timeout");
        exchange(&mut stream);
    })
}

fn exchange(stream: &mut UnixStream) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("client read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("client write timeout");
    stream.write_all(b"ping").expect("write ping");
    let mut echoed = [0u8; 4];
    stream.read_exact(&mut echoed).expect("read ping");
    assert_eq!(&echoed, b"ping");
}

/// A spec whose sandbox can re-run this test binary as an in-sandbox client.
fn client_spec(lab: &support::LabFixture) -> SandboxSpec {
    let mut spec = lab.spec.clone();
    let exe = std::env::current_exe().expect("test binary path");
    let deps = exe.parent().expect("test binary directory").to_path_buf();
    spec.ro_binds.push(deps);
    spec
}

fn helper_request(mode: &str) -> ExecRequest {
    let exe = std::env::current_exe().expect("test binary path");
    ExecRequest::new(vec![
        exe.to_string_lossy().into_owned(),
        "--exact".to_string(),
        "hub_client_helper".to_string(),
        "--nocapture".to_string(),
    ])
    .with_env(CLIENT_MODE, mode)
    .with_timeout(Duration::from_secs(20))
}

fn run_helper(handle: &SandboxHandle, mode: &str) {
    run_request(NamespaceBackend::new(), handle, helper_request(mode));
}

fn run_request(backend: NamespaceBackend, handle: &SandboxHandle, request: ExecRequest) {
    let output = backend.exec(handle, &request).expect("exec client helper");
    assert_eq!(
        output.exit_code,
        Some(0),
        "client failed:\nstdout: {}\nstderr: {}",
        output.stdout_text(),
        output.stderr_text()
    );
    assert!(
        output.stdout_text().contains("CLIENT-OK"),
        "client did not confirm the exchange: {}",
        output.stdout_text()
    );
}

/// The in-sandbox client, run through the test binary re-executed in the sandbox.
#[test]
fn hub_client_helper() {
    let Ok(mode) = std::env::var(CLIENT_MODE) else {
        return;
    };
    match mode.as_str() {
        "exchange" => {
            assert_eq!(
                std::env::var("JG_HUB_SOCKET").expect("hub env"),
                HUB_SOCKET_PATH
            );
            let mut stream = UnixStream::connect(HUB_SOCKET_PATH).expect("connect hub socket");
            exchange(&mut stream);
        }
        "absent" => {
            assert!(std::env::var_os("JG_HUB_SOCKET").is_none());
            assert!(!Path::new(HUB_SOCKET_PATH).exists());
        }
        "both" => {
            assert_eq!(
                std::env::var("JG_HUB_SOCKET").expect("hub env"),
                HUB_SOCKET_PATH
            );
            assert_eq!(
                std::env::var("HTTP_PROXY").expect("proxy env"),
                format!("http://127.0.0.1:{DEFAULT_EGRESS_PORT}")
            );
            let mut hub = UnixStream::connect(HUB_SOCKET_PATH).expect("connect hub socket");
            exchange(&mut hub);
            let mut egress =
                UnixStream::connect(EGRESS_SOCKET_PATH).expect("connect egress socket");
            exchange(&mut egress);
        }
        other => panic!("unknown client mode {other}"),
    }
    println!("CLIENT-OK");
}

#[test]
fn hub_socket_is_visible_and_relays_bytes() {
    let lab = fixture();
    let socket_path = lab.temp.path().join("hub.sock");
    let server = spawn_echo_server(&socket_path);

    let mut spec = client_spec(&lab);
    spec.hub_socket = Some(socket_path.clone());
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &spec) else {
        return;
    };

    run_helper(guard.handle(), "exchange");
    server.join().expect("echo server thread");
}

#[test]
fn scoped_exec_keeps_the_hub_socket_visible() {
    let lab = fixture();
    std::fs::create_dir_all(lab.spec.lab_dir.join("proj-a")).expect("worktree");
    let socket_path = lab.temp.path().join("hub.sock");
    let server = spawn_echo_server(&socket_path);

    let mut spec = client_spec(&lab);
    spec.hub_socket = Some(socket_path.clone());
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &spec) else {
        return;
    };

    let scope = ExecScope::new(ScopeAccess::ReadWrite, "/lab/proj-a");
    let request = helper_request("exchange").with_scope(scope);
    run_request(backend, guard.handle(), request);
    server.join().expect("echo server thread");
}

#[test]
fn sandbox_without_hub_socket_has_no_socket_or_env() {
    let lab = fixture();
    // Egress creates `/run/jg`; the hub path must still be absent without it.
    let egress_path = lab.temp.path().join("egress.sock");
    let _egress = UnixListener::bind(&egress_path).expect("bind egress socket");

    let mut spec = client_spec(&lab);
    spec.egress = Some(EgressSpec::new(&egress_path));
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &spec) else {
        return;
    };

    run_helper(guard.handle(), "absent");
}

#[test]
fn non_socket_hub_path_fails_closed() {
    let lab = fixture();
    let regular = lab.temp.path().join("not-a-socket");
    std::fs::write(&regular, b"not a socket").expect("write regular file");

    let mut spec = lab.spec.clone();
    spec.hub_socket = Some(regular);
    let state_dir = spec.state_dir.clone();

    let backend = NamespaceBackend::new();
    match backend.create(&spec) {
        Err(SandboxError::InvalidSpec { .. }) => {}
        other => panic!("non-socket hub path must be rejected, got {other:?}"),
    }
    assert!(
        !state_dir.exists(),
        "the state directory was created before hub validation"
    );
}

#[test]
fn hub_and_egress_sockets_work_together() {
    let lab = fixture();
    let hub_path = lab.temp.path().join("hub.sock");
    let egress_path = lab.temp.path().join("egress.sock");
    let hub_server = spawn_echo_server(&hub_path);
    let egress_server = spawn_echo_server(&egress_path);

    let mut spec = client_spec(&lab);
    spec.hub_socket = Some(hub_path.clone());
    spec.egress = Some(EgressSpec::new(&egress_path));
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &spec) else {
        return;
    };

    run_helper(guard.handle(), "both");
    hub_server.join().expect("hub echo server thread");
    egress_server.join().expect("egress echo server thread");
}

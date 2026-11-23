//! Integration tests for sandbox egress wiring and the in-sandbox bridge.

mod support;

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use jg_sandbox::{
    Backend, DEFAULT_EGRESS_PORT, EgressEvent, EgressPolicy, EgressProxy, EgressSpec, ExecRequest,
    NamespaceBackend, SandboxError, SandboxHandle,
};
use support::{create_or_skip, fixture, run_ok};

/// A public-looking address used by the injected resolver.
const VETTED_PUBLIC: &str = "93.184.216.34";
/// Marker read by the helper test when it runs inside a sandbox.
const CLIENT_MODE: &str = "JG_SANDBOX_CLIENT_MODE";
/// Loopback port handed to the helper test.
const CLIENT_PORT: &str = "JG_EGRESS_PORT";

#[derive(Clone, Default)]
struct AuditLog {
    events: Arc<Mutex<Vec<EgressEvent>>>,
}

impl AuditLog {
    fn hook(&self) -> Arc<dyn Fn(EgressEvent) + Send + Sync> {
        let events = Arc::clone(&self.events);
        Arc::new(move |event| events.lock().expect("audit lock").push(event))
    }

    fn snapshot(&self) -> Vec<EgressEvent> {
        self.events.lock().expect("audit lock").clone()
    }
}

fn vetted_public() -> IpAddr {
    VETTED_PUBLIC.parse().expect("valid public address")
}

/// A TCP echo server standing in for one allowed host.
fn spawn_echo_target() -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind echo target");
    let address = listener.local_addr().expect("echo address");
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept echo connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("echo read timeout");
        let mut payload = [0u8; 4];
        stream.read_exact(&mut payload).expect("read echo bytes");
        stream.write_all(&payload).expect("write echo bytes");
        let _ = stream.shutdown(std::net::Shutdown::Both);
    });
    (address, handle)
}

struct ProxyFixture {
    proxy: EgressProxy,
    audit: AuditLog,
    target: thread::JoinHandle<()>,
}

/// Start a proxy whose resolver maps every allowed host to a public address and
/// whose connector reaches the local echo server.
fn start_proxy(socket_path: &Path, policy_domains: &[&str]) -> ProxyFixture {
    let policy = EgressPolicy::new(policy_domains.iter().copied()).expect("valid policy");
    let audit = AuditLog::default();
    let (target_address, target) = spawn_echo_target();
    let resolver =
        move |_host: &str, _port: u16| -> io::Result<Vec<IpAddr>> { Ok(vec![vetted_public()]) };
    let connector = move |_address: SocketAddr, timeout: Duration| -> io::Result<TcpStream> {
        TcpStream::connect_timeout(&target_address, timeout)
    };
    let proxy = EgressProxy::start_with(socket_path, policy, audit.hook(), resolver, connector)
        .expect("start egress proxy");
    ProxyFixture {
        proxy,
        audit,
        target,
    }
}

fn state_dir(lab: &support::LabFixture) -> std::path::PathBuf {
    lab.spec.state_dir.clone()
}

/// The spec for a sandbox that can run this test binary as an in-sandbox client.
fn client_spec(lab: &support::LabFixture) -> jg_sandbox::SandboxSpec {
    let mut spec = lab.spec.clone();
    let exe = std::env::current_exe().expect("test binary path");
    let deps = exe.parent().expect("test binary directory").to_path_buf();
    spec.ro_binds.push(deps);
    spec
}

fn run_client(handle: &SandboxHandle, mode: &str) -> String {
    let exe = std::env::current_exe().expect("test binary path");
    let request = ExecRequest::new(vec![
        exe.to_string_lossy().into_owned(),
        "--exact".to_string(),
        "sandbox_client_helper".to_string(),
        "--nocapture".to_string(),
    ])
    .with_env(CLIENT_MODE, mode)
    .with_env(CLIENT_PORT, DEFAULT_EGRESS_PORT.to_string())
    .with_timeout(Duration::from_secs(20));
    let output = NamespaceBackend::new()
        .exec(handle, &request)
        .expect("exec client helper");
    assert_eq!(
        output.exit_code,
        Some(0),
        "client {mode} failed:\nstdout: {}\nstderr: {}",
        output.stdout_text(),
        output.stderr_text()
    );
    output.stdout_text()
}

/// The in-sandbox client, run through the test binary re-executed in the sandbox.
#[test]
fn sandbox_client_helper() {
    let Ok(mode) = std::env::var(CLIENT_MODE) else {
        return;
    };
    let port: u16 = std::env::var(CLIENT_PORT)
        .expect("client port")
        .parse()
        .expect("valid client port");
    match mode.as_str() {
        "allowed" => connect_through_bridge(port, "allowed.test", 200, true),
        "blocked" => connect_through_bridge(port, "blocked.test", 403, false),
        "closed" => {
            assert!(
                TcpStream::connect_timeout(
                    &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
                    Duration::from_millis(800),
                )
                .is_err(),
                "unexpected loopback listener on port {port}"
            );
        }
        "direct" => {
            let target: SocketAddr = "93.184.216.34:443".parse().expect("public target");
            assert!(
                TcpStream::connect_timeout(&target, Duration::from_secs(3)).is_err(),
                "a non-loopback connection succeeded without egress routing"
            );
        }
        other => panic!("unknown client mode {other}"),
    }
    println!("CLIENT-OK");
}

fn connect_through_bridge(port: u16, host: &str, expected: u16, tunnel: bool) {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream =
        TcpStream::connect_timeout(&address, Duration::from_secs(5)).expect("connect bridge");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("bridge read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("bridge write timeout");
    stream
        .write_all(format!("CONNECT {host}:443 HTTP/1.1\r\nHost: {host}:443\r\n\r\n").as_bytes())
        .expect("write CONNECT");
    let status = read_status(&mut stream);
    assert_eq!(status, expected, "CONNECT {host} status");
    if !tunnel {
        return;
    }
    stream.write_all(b"ping").expect("write tunnel bytes");
    let mut echoed = [0u8; 4];
    stream.read_exact(&mut echoed).expect("read tunnel bytes");
    assert_eq!(&echoed, b"ping");
}

fn read_status(stream: &mut TcpStream) -> u16 {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 512];
    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut chunk).expect("read proxy response");
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8_lossy(&buffer)
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

#[test]
fn egress_bridge_relays_allowed_and_refuses_others() {
    let lab = fixture();
    let socket_path = lab.temp.path().join("egress.sock");
    let proxy = start_proxy(&socket_path, &["allowed.test"]);

    let mut spec = client_spec(&lab);
    spec.egress = Some(EgressSpec::new(&socket_path));
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &spec) else {
        return;
    };

    run_client(guard.handle(), "allowed");
    run_client(guard.handle(), "blocked");
    run_client(guard.handle(), "direct");

    let events = proxy.audit.snapshot();
    assert!(
        events
            .iter()
            .any(|event| event.host == "allowed.test" && event.allowed),
        "missing allowed audit event: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event.host == "blocked.test" && !event.allowed),
        "missing blocked audit event: {events:?}"
    );

    drop(guard);
    proxy.target.join().expect("echo target thread");
    drop(proxy.proxy);
}

#[test]
fn egress_sets_proxy_env_and_request_env_wins() {
    let lab = fixture();
    let socket_path = lab.temp.path().join("egress.sock");
    let _proxy = start_proxy(&socket_path, &["allowed.test"]);

    let mut spec = client_spec(&lab);
    spec.egress = Some(EgressSpec::new(&socket_path));
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &spec) else {
        return;
    };

    let script = concat!(
        "printf 'HTTP_PROXY=%s\\n' \"${HTTP_PROXY:-}\"\n",
        "printf 'HTTPS_PROXY=%s\\n' \"${HTTPS_PROXY:-}\"\n",
        "printf 'ALL_PROXY=%s\\n' \"${ALL_PROXY:-}\"\n",
        "printf 'http_proxy=%s\\n' \"${http_proxy:-}\"\n",
        "printf 'NO_PROXY=%s\\n' \"${NO_PROXY:-}\"\n",
    );
    let output = run_ok(backend, guard.handle(), script);
    let text = output.stdout_text();
    let expected = format!("http://127.0.0.1:{DEFAULT_EGRESS_PORT}");
    for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy"] {
        assert!(
            text.contains(&format!("{key}={expected}")),
            "{key} missing from {text}"
        );
    }
    assert!(
        text.contains("NO_PROXY=localhost,127.0.0.1"),
        "NO_PROXY missing from {text}"
    );

    let request = ExecRequest::new(vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "printf '%s' \"${HTTP_PROXY:-unset}\"".to_string(),
    ])
    .with_env("HTTP_PROXY", "http://request-proxy");
    let output = backend.exec(guard.handle(), &request).expect("exec");
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout_text(), "http://request-proxy");
}

#[test]
fn sandbox_without_egress_has_no_bridge_or_proxy_env() {
    let lab = fixture();
    let spec = client_spec(&lab);
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &spec) else {
        return;
    };

    run_client(guard.handle(), "closed");

    let script = concat!(
        "printf 'HTTP_PROXY=%s\\n' \"${HTTP_PROXY:-}\"\n",
        "printf 'HTTPS_PROXY=%s\\n' \"${HTTPS_PROXY:-}\"\n",
        "printf 'ALL_PROXY=%s\\n' \"${ALL_PROXY:-}\"\n",
        "printf 'http_proxy=%s\\n' \"${http_proxy:-}\"\n",
        "printf 'NO_PROXY=%s\\n' \"${NO_PROXY:-}\"\n",
    );
    let output = run_ok(backend, guard.handle(), script);
    for line in output.stdout_text().lines() {
        assert!(
            line.ends_with('='),
            "unexpected proxy environment without egress: {line}"
        );
    }
}

#[test]
fn egress_with_a_non_socket_proxy_path_fails_closed() {
    let lab = fixture();
    let regular = lab.temp.path().join("not-a-socket");
    std::fs::write(&regular, b"not a socket").expect("write regular file");

    let mut spec = lab.spec.clone();
    spec.egress = Some(EgressSpec::new(regular));
    let state_dir = state_dir(&lab);

    let backend = NamespaceBackend::new();
    match backend.create(&spec) {
        Err(SandboxError::InvalidSpec { .. }) => {}
        other => panic!("non-socket egress proxy must be rejected, got {other:?}"),
    }
    assert!(
        !state_dir.exists(),
        "the state directory was created before egress validation"
    );
}

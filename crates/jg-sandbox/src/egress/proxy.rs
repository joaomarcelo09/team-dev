//! A host-side HTTP/1.1 `CONNECT` proxy enforcing an [`EgressPolicy`].
//!
//! The proxy listens on a Unix socket inside the sandbox bridge, checks every
//! requested `host:port` against the policy, resolves the name on the host,
//! refuses non-public addresses, and only then opens the TCP connection. The
//! sandbox never gets a raw network stack of its own.

use std::fs;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::policy::{EgressPolicy, is_valid_hostname};
use crate::error::{Result, SandboxError};

/// Largest request head the proxy accepts, in bytes.
const MAX_HEADER_BYTES: usize = 8 * 1024;
/// Connections handled at the same time before new ones are refused.
const MAX_CONNECTIONS: usize = 128;
/// How long the proxy waits for the request head.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a TCP connect may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an idle tunnel may stay open.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the accept loop checks the stop flag while it is non-blocking.
const ACCEPT_POLL: Duration = Duration::from_millis(10);

/// One egress decision, reported to the audit hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressEvent {
    /// Requested host name; empty when the request could not be parsed.
    pub host: String,
    /// Requested port; zero when the request could not be parsed.
    pub port: u16,
    /// Whether the CONNECT was allowed and tunnelled.
    pub allowed: bool,
    /// Why the request was allowed or refused.
    pub reason: String,
}

/// How a destination address is classified before connecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressClass {
    /// A globally routable address, the only class the proxy connects to.
    Public,
    /// Loopback (`127.0.0.0/8`, `::1`).
    Loopback,
    /// RFC 1918 private (`10/8`, `172.16/12`, `192.168/16`).
    Private,
    /// Link-local (`169.254/16`, `fe80::/10`), including cloud metadata.
    LinkLocal,
    /// Carrier-grade NAT (`100.64/10`).
    Shared,
    /// Unspecified address (`0.0.0.0`, `::`).
    Unspecified,
    /// Multicast address.
    Multicast,
    /// IPv4 broadcast address.
    Broadcast,
    /// IPv6 unique local address (`fc00::/7`).
    UniqueLocal,
    /// IPv4-mapped IPv6 address (`::ffff:a.b.c.d`).
    V4Mapped,
    /// Any other special-purpose address the proxy refuses.
    Reserved,
}

impl AddressClass {
    /// Whether the proxy may connect to an address of this class.
    pub fn is_public(self) -> bool {
        matches!(self, Self::Public)
    }

    /// A short lowercase name used in audit reasons.
    pub fn label(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Loopback => "loopback",
            Self::Private => "private",
            Self::LinkLocal => "link-local",
            Self::Shared => "carrier-grade NAT",
            Self::Unspecified => "unspecified",
            Self::Multicast => "multicast",
            Self::Broadcast => "broadcast",
            Self::UniqueLocal => "unique local",
            Self::V4Mapped => "IPv4-mapped",
            Self::Reserved => "reserved",
        }
    }
}

/// Classify an address before the proxy is allowed to connect to it.
pub fn classify_address(address: IpAddr) -> AddressClass {
    match address {
        IpAddr::V4(address) => classify_ipv4(address),
        IpAddr::V6(address) => classify_ipv6(address),
    }
}

fn classify_ipv4(address: Ipv4Addr) -> AddressClass {
    let [a, b, c, _d] = address.octets();

    if address.is_unspecified() {
        return AddressClass::Unspecified;
    }
    if address.is_loopback() {
        return AddressClass::Loopback;
    }
    if address.is_private() {
        return AddressClass::Private;
    }
    if address.is_link_local() {
        return AddressClass::LinkLocal;
    }
    if address.is_multicast() {
        return AddressClass::Multicast;
    }
    if address.is_broadcast() {
        return AddressClass::Broadcast;
    }
    if a == 100 && (64..=127).contains(&b) {
        return AddressClass::Shared;
    }
    if a >= 240 || a == 0 {
        return AddressClass::Reserved;
    }
    // Documentation, benchmarking and the IETF protocol assignments are not
    // routable destinations; refuse them instead of treating them as public.
    if (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
    {
        return AddressClass::Reserved;
    }
    AddressClass::Public
}

fn classify_ipv6(address: Ipv6Addr) -> AddressClass {
    if address.is_unspecified() {
        return AddressClass::Unspecified;
    }
    if address.is_loopback() {
        return AddressClass::Loopback;
    }
    if address.to_ipv4_mapped().is_some() {
        return AddressClass::V4Mapped;
    }
    if address.is_multicast() {
        return AddressClass::Multicast;
    }

    let segments = address.segments();
    if segments[0] & 0xffc0 == 0xfe80 {
        return AddressClass::LinkLocal;
    }
    if segments[0] & 0xfe00 == 0xfc00 {
        return AddressClass::UniqueLocal;
    }
    // Only 2000::/3 is global unicast; everything else is special-purpose.
    if segments[0] & 0xe000 == 0x2000 {
        if segments[0] == 0x2001 && segments[1] == 0x0db8 {
            return AddressClass::Reserved;
        }
        return AddressClass::Public;
    }
    AddressClass::Reserved
}

/// Resolves requested host names into IP addresses.
///
/// The default implementation uses the host resolver. Tests inject a resolver
/// so they never touch the network.
pub trait Resolver: Send + Sync {
    /// Resolve `host` for `port`.
    fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<IpAddr>>;
}

impl<F> Resolver for F
where
    F: Fn(&str, u16) -> io::Result<Vec<IpAddr>> + Send + Sync,
{
    fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<IpAddr>> {
        self(host, port)
    }
}

/// The host system resolver.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<IpAddr>> {
        Ok((host, port)
            .to_socket_addrs()?
            .map(|address| address.ip())
            .collect())
    }
}

/// Opens the TCP connection to an address the proxy already vetted.
///
/// Tests inject a connector so a vetted public address can point at a local
/// test server without weakening the address checks.
pub trait Connector: Send + Sync {
    /// Connect to `address`, giving up after `timeout`.
    fn connect(&self, address: SocketAddr, timeout: Duration) -> io::Result<TcpStream>;
}

impl<F> Connector for F
where
    F: Fn(SocketAddr, Duration) -> io::Result<TcpStream> + Send + Sync,
{
    fn connect(&self, address: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
        self(address, timeout)
    }
}

/// The default connector: a plain TCP connection with a timeout.
#[derive(Debug, Clone, Copy, Default)]
pub struct TcpConnector;

impl Connector for TcpConnector {
    fn connect(&self, address: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
        TcpStream::connect_timeout(&address, timeout)
    }
}

/// A running host-side egress proxy.
pub struct EgressProxy {
    socket_path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl EgressProxy {
    /// Bind the proxy at `socket_path` and start serving in a background thread.
    pub fn start(
        socket_path: impl AsRef<Path>,
        policy: EgressPolicy,
        audit: Arc<dyn Fn(EgressEvent) + Send + Sync>,
    ) -> Result<Self> {
        Self::start_with_resolver(socket_path, policy, audit, SystemResolver)
    }

    /// Start with an injected resolver, keeping the default TCP connector.
    pub fn start_with_resolver<R>(
        socket_path: impl AsRef<Path>,
        policy: EgressPolicy,
        audit: Arc<dyn Fn(EgressEvent) + Send + Sync>,
        resolver: R,
    ) -> Result<Self>
    where
        R: Resolver + 'static,
    {
        Self::start_with(socket_path, policy, audit, resolver, TcpConnector)
    }

    /// Start with an injected resolver and connector; meant for tests.
    pub fn start_with<R, C>(
        socket_path: impl AsRef<Path>,
        policy: EgressPolicy,
        audit: Arc<dyn Fn(EgressEvent) + Send + Sync>,
        resolver: R,
        connector: C,
    ) -> Result<Self>
    where
        R: Resolver + 'static,
        C: Connector + 'static,
    {
        let socket_path = socket_path.as_ref().to_path_buf();
        prepare_socket(&socket_path)?;

        let listener = UnixListener::bind(&socket_path)
            .map_err(|error| SandboxError::io(&socket_path, error))?;
        if let Err(error) = fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(&socket_path);
            return Err(SandboxError::io(&socket_path, error));
        }
        if let Err(error) = listener.set_nonblocking(true) {
            let _ = fs::remove_file(&socket_path);
            return Err(SandboxError::io(&socket_path, error));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(ProxyState {
            policy,
            audit,
            resolver: Arc::new(resolver),
            connector: Arc::new(connector),
            stop: Arc::clone(&stop),
            active: Arc::new(AtomicUsize::new(0)),
        });
        let thread = thread::spawn(move || accept_loop(listener, state));

        Ok(Self {
            socket_path,
            stop,
            thread: Some(thread),
        })
    }

    /// The Unix socket the proxy is listening on.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Stop accepting connections and remove the socket.
    pub fn stop(&mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

impl Drop for EgressProxy {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct ProxyState {
    policy: EgressPolicy,
    audit: Arc<dyn Fn(EgressEvent) + Send + Sync>,
    resolver: Arc<dyn Resolver>,
    connector: Arc<dyn Connector>,
    stop: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
}

impl ProxyState {
    fn audit(&self, host: &str, port: u16, allowed: bool, reason: impl Into<String>) {
        (self.audit)(EgressEvent {
            host: host.to_string(),
            port,
            allowed,
            reason: reason.into(),
        });
    }
}

fn prepare_socket(socket_path: &Path) -> Result<()> {
    if let Some(parent) = socket_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|error| SandboxError::io(parent, error))?;
    }

    let metadata = match fs::symlink_metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(SandboxError::io(socket_path, error)),
    };
    if !metadata.file_type().is_socket() {
        return Err(SandboxError::AlreadyExists(socket_path.to_path_buf()));
    }
    match UnixStream::connect(socket_path) {
        Ok(_) => Err(SandboxError::AlreadyExists(socket_path.to_path_buf())),
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
            fs::remove_file(socket_path).map_err(|error| SandboxError::io(socket_path, error))
        }
        Err(_) => Err(SandboxError::AlreadyExists(socket_path.to_path_buf())),
    }
}

fn accept_loop(listener: UnixListener, state: Arc<ProxyState>) {
    while !state.stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                if state.active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                    let mut stream = stream;
                    respond(&mut stream, "503 Service Unavailable");
                    continue;
                }
                state.active.fetch_add(1, Ordering::SeqCst);
                let worker_state = Arc::clone(&state);
                thread::spawn(move || {
                    let _guard = ActiveGuard(Arc::clone(&worker_state.active));
                    handle_connection(stream, &worker_state);
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL);
            }
            Err(_) => break,
        }
    }
}

struct ActiveGuard(Arc<AtomicUsize>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn handle_connection(mut client: UnixStream, state: &ProxyState) {
    let _ = client.set_read_timeout(Some(HEADER_TIMEOUT));
    let _ = client.set_write_timeout(Some(HEADER_TIMEOUT));

    let (head, leftover) = match read_head(&mut client) {
        Ok(value) => value,
        Err(error) => {
            state.audit("", 0, false, format!("invalid request: {error}"));
            respond(&mut client, "400 Bad Request");
            return;
        }
    };

    let target = match parse_request(&head) {
        Ok(target) => target,
        Err(RequestError::MethodNotAllowed) => {
            state.audit("", 0, false, "only CONNECT is supported");
            respond_method_not_allowed(&mut client);
            return;
        }
        Err(RequestError::BadRequest(reason)) => {
            state.audit("", 0, false, reason);
            respond(&mut client, "400 Bad Request");
            return;
        }
    };

    let (host, port) = target.host_port();
    if target.is_ip_literal() {
        state.audit(host, port, false, "IP literal hosts are refused");
        respond(&mut client, "403 Forbidden");
        return;
    }

    if !state.policy.allows(host, port) {
        let reason = if state.policy.allows_host(host) {
            "port not in the egress allowlist"
        } else {
            "host not in the egress allowlist"
        };
        state.audit(host, port, false, reason);
        respond(&mut client, "403 Forbidden");
        return;
    }

    let addresses = match state.resolver.resolve(host, port) {
        Ok(addresses) => addresses,
        Err(error) => {
            state.audit(host, port, false, format!("resolution failed: {error}"));
            respond(&mut client, "502 Bad Gateway");
            return;
        }
    };
    if addresses.is_empty() {
        state.audit(host, port, false, "host resolved to no addresses");
        respond(&mut client, "502 Bad Gateway");
        return;
    }
    for address in &addresses {
        let class = classify_address(*address);
        if !class.is_public() {
            state.audit(
                host,
                port,
                false,
                format!(
                    "host resolved to refused address {address} ({})",
                    class.label()
                ),
            );
            respond(&mut client, "403 Forbidden");
            return;
        }
    }

    let address = SocketAddr::new(addresses[0], port);
    let mut upstream = match state.connector.connect(address, CONNECT_TIMEOUT) {
        Ok(stream) => stream,
        Err(error) => {
            state.audit(host, port, false, format!("connect failed: {error}"));
            respond(&mut client, "502 Bad Gateway");
            return;
        }
    };

    respond_established(&mut client);
    if !leftover.is_empty() && upstream.write_all(&leftover).is_err() {
        state.audit(host, port, false, "failed to forward buffered request data");
        return;
    }
    state.audit(host, port, true, "allowed");

    let _ = client.set_read_timeout(Some(IDLE_TIMEOUT));
    let _ = client.set_write_timeout(Some(IDLE_TIMEOUT));
    let _ = upstream.set_read_timeout(Some(IDLE_TIMEOUT));
    let _ = upstream.set_write_timeout(Some(IDLE_TIMEOUT));
    relay(client, upstream);
}

/// Copy bytes in both directions until either side closes or goes idle.
fn relay(client: UnixStream, upstream: TcpStream) {
    let Ok(mut client_read) = client.try_clone() else {
        return;
    };
    let Ok(mut upstream_write) = upstream.try_clone() else {
        return;
    };
    let client_thread = thread::spawn(move || {
        let result = io::copy(&mut client_read, &mut upstream_write);
        let _ = upstream_write.shutdown(Shutdown::Write);
        result
    });

    let mut upstream_read = upstream;
    let mut client_write = client;
    let _ = io::copy(&mut upstream_read, &mut client_write);
    let _ = client_write.shutdown(Shutdown::Both);
    let _ = upstream_read.shutdown(Shutdown::Both);
    let _ = client_thread.join();
}

fn respond(stream: &mut UnixStream, status: &str) {
    respond_with(stream, status, "");
}

fn respond_established(stream: &mut UnixStream) {
    let _ = stream.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n");
    let _ = stream.flush();
}

fn respond_method_not_allowed(stream: &mut UnixStream) {
    respond_with(stream, "405 Method Not Allowed", "Allow: CONNECT\r\n");
}

fn respond_with(stream: &mut UnixStream, status: &str, extra_headers: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n{extra_headers}\r\n"
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn read_head(stream: &mut UnixStream) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut buffer = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(io::Error::other("request head is too large"));
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before the request head",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = header_end(&buffer) {
            if position > MAX_HEADER_BYTES {
                return Err(io::Error::other("request head is too large"));
            }
            let leftover = buffer.split_off(position + 4);
            buffer.truncate(position);
            return Ok((buffer, leftover));
        }
    }
}

fn header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

#[derive(Debug)]
enum RequestError {
    BadRequest(&'static str),
    MethodNotAllowed,
}

enum Target {
    Hostname { host: String, port: u16 },
    IpLiteral { host: String, port: u16 },
}

impl Target {
    fn host_port(&self) -> (&str, u16) {
        match self {
            Self::Hostname { host, port } | Self::IpLiteral { host, port } => (host, *port),
        }
    }

    fn is_ip_literal(&self) -> bool {
        matches!(self, Self::IpLiteral { .. })
    }
}

fn parse_request(head: &[u8]) -> std::result::Result<Target, RequestError> {
    let text = std::str::from_utf8(head)
        .map_err(|_| RequestError::BadRequest("request head is not UTF-8"))?;
    let mut lines = text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or(RequestError::BadRequest("missing request line"))?;
    let parts: Vec<&str> = request_line.split(' ').collect();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        return Err(RequestError::BadRequest("malformed request line"));
    }
    if parts[0] != "CONNECT" {
        return Err(RequestError::MethodNotAllowed);
    }
    if parts[2] != "HTTP/1.1" && parts[2] != "HTTP/1.0" {
        return Err(RequestError::BadRequest("unsupported HTTP version"));
    }
    if lines.any(|line| line.contains('\r') || line.contains('\n')) {
        return Err(RequestError::BadRequest("malformed header line"));
    }
    parse_target(parts[1])
}

fn parse_target(target: &str) -> std::result::Result<Target, RequestError> {
    if target.is_empty()
        || target.contains('/')
        || target.contains('@')
        || target.contains('?')
        || target.contains('#')
        || target.chars().any(char::is_whitespace)
    {
        return Err(RequestError::BadRequest("invalid CONNECT target"));
    }

    if target.starts_with('[') {
        let end = target
            .find(']')
            .ok_or(RequestError::BadRequest("invalid IPv6 authority"))?;
        let port = target[end + 1..]
            .strip_prefix(':')
            .ok_or(RequestError::BadRequest("missing port"))?;
        let port = parse_port(port)?;
        return Ok(Target::IpLiteral {
            host: target[..=end].to_string(),
            port,
        });
    }

    let (host, port) = target
        .rsplit_once(':')
        .ok_or(RequestError::BadRequest("missing port"))?;
    let port = parse_port(port)?;
    if host.is_empty() {
        return Err(RequestError::BadRequest("missing host"));
    }
    if host.contains(':') || host.parse::<IpAddr>().is_ok() {
        return Ok(Target::IpLiteral {
            host: host.to_string(),
            port,
        });
    }
    if !is_valid_hostname(host) {
        return Err(RequestError::BadRequest("invalid host"));
    }
    Ok(Target::Hostname {
        host: host.to_ascii_lowercase(),
        port,
    })
}

fn parse_port(value: &str) -> std::result::Result<u16, RequestError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(RequestError::BadRequest("invalid port"));
    }
    let port: u16 = value
        .parse()
        .map_err(|_| RequestError::BadRequest("invalid port"))?;
    if port == 0 {
        return Err(RequestError::BadRequest("invalid port"));
    }
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("valid IP address")
    }

    #[test]
    fn public_addresses_are_allowed() {
        for address in ["93.184.216.34", "1.1.1.1", "2606:4700:4700::1111"] {
            assert_eq!(
                classify_address(ip(address)),
                AddressClass::Public,
                "{address}"
            );
        }
    }

    #[test]
    fn loopback_is_refused() {
        for address in ["127.0.0.1", "127.255.255.254", "::1"] {
            assert_eq!(
                classify_address(ip(address)),
                AddressClass::Loopback,
                "{address}"
            );
        }
    }

    #[test]
    fn private_ranges_are_refused() {
        for address in ["10.0.0.1", "10.255.255.255", "172.16.0.1", "172.31.255.254"] {
            assert_eq!(
                classify_address(ip(address)),
                AddressClass::Private,
                "{address}"
            );
        }
        for address in ["192.168.0.1", "192.168.1.8"] {
            assert_eq!(
                classify_address(ip(address)),
                AddressClass::Private,
                "{address}"
            );
        }
    }

    #[test]
    fn link_local_and_metadata_are_refused() {
        for address in ["169.254.0.1", "169.254.169.254", "fe80::1"] {
            assert_eq!(
                classify_address(ip(address)),
                AddressClass::LinkLocal,
                "{address}"
            );
        }
    }

    #[test]
    fn cgnat_is_refused() {
        for address in ["100.64.0.1", "100.127.255.254"] {
            assert_eq!(
                classify_address(ip(address)),
                AddressClass::Shared,
                "{address}"
            );
        }
    }

    #[test]
    fn unspecified_multicast_and_broadcast_are_refused() {
        assert_eq!(classify_address(ip("0.0.0.0")), AddressClass::Unspecified);
        assert_eq!(classify_address(ip("::")), AddressClass::Unspecified);
        assert_eq!(classify_address(ip("224.0.0.1")), AddressClass::Multicast);
        assert_eq!(classify_address(ip("ff02::1")), AddressClass::Multicast);
        assert_eq!(
            classify_address(ip("255.255.255.255")),
            AddressClass::Broadcast
        );
    }

    #[test]
    fn ipv6_special_ranges_are_refused() {
        assert_eq!(classify_address(ip("fc00::1")), AddressClass::UniqueLocal);
        assert_eq!(
            classify_address(ip("fd12:3456::1")),
            AddressClass::UniqueLocal
        );
        assert_eq!(
            classify_address(ip("::ffff:127.0.0.1")),
            AddressClass::V4Mapped
        );
        assert_eq!(classify_address(ip("2001:db8::1")), AddressClass::Reserved);
        assert_eq!(classify_address(ip("64:ff9b::1")), AddressClass::Reserved);
    }

    #[test]
    fn reserved_ipv4_ranges_are_refused() {
        for address in ["0.1.2.3", "198.51.100.1", "203.0.113.1", "240.0.0.1"] {
            assert_eq!(
                classify_address(ip(address)),
                AddressClass::Reserved,
                "{address}"
            );
        }
    }

    #[test]
    fn parses_valid_connect_targets() {
        let Target::Hostname { host, port } =
            parse_target("Registry.NPMJS.org:443").expect("valid target")
        else {
            panic!("expected a hostname");
        };
        assert_eq!(host, "registry.npmjs.org");
        assert_eq!(port, 443);

        assert!(
            parse_target("127.0.0.1:443")
                .expect("literal")
                .is_ip_literal()
        );
        assert!(parse_target("[::1]:443").expect("literal").is_ip_literal());
    }

    #[test]
    fn rejects_malformed_targets() {
        for target in [
            "",
            "registry.npmjs.org",
            "registry.npmjs.org:0",
            "registry.npmjs.org:notaport",
            "registry.npmjs.org:99999",
            "http://registry.npmjs.org:443",
            "registry.npmjs.org/path:443",
            "user@registry.npmjs.org:443",
            "exa_mple.com:443",
        ] {
            assert!(parse_target(target).is_err(), "{target:?}");
        }
    }

    #[test]
    fn rejects_oversized_heads_and_other_methods() {
        let big = vec![b'A'; MAX_HEADER_BYTES + 1];
        assert!(parse_request(&big).is_err());
        assert!(matches!(
            parse_request(b"GET / HTTP/1.1\r\n\r\n"),
            Err(RequestError::MethodNotAllowed)
        ));
        assert!(matches!(
            parse_request(b"CONNECT host:443 HTTP/2\r\n\r\n"),
            Err(RequestError::BadRequest(_))
        ));
    }
}

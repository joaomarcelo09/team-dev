//! A single command run inside a sandbox.

use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{collections::BTreeMap, fmt};

use crate::scope::{ExecScope, SCOPED_HOME, WORKSPACE};

/// Default `PATH` given to commands when the request does not override it.
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// Default `HOME` inside the sandbox (inside `/lab`, so it persists).
pub const DEFAULT_HOME: &str = "/lab/home";
/// Default locale used when the request does not override it.
pub const DEFAULT_LANG: &str = "C.UTF-8";

/// A command to run inside a sandbox.
#[derive(Clone, PartialEq, Eq)]
pub struct ExecRequest {
    /// `argv[0]` is the program, looked up in `PATH` when it has no `/`.
    pub argv: Vec<String>,
    /// Extra environment variables; the host environment is never inherited.
    pub env: BTreeMap<String, String>,
    /// Working directory inside the sandbox.
    pub cwd: PathBuf,
    /// Optional bytes written to the command's standard input.
    pub stdin: Option<Vec<u8>>,
    /// Optional wall-clock limit after which the command is killed.
    pub timeout: Option<Duration>,
    /// Optional project scope; `None` runs lab-wide, as before.
    pub scope: Option<ExecScope>,
}

impl fmt::Debug for ExecRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecRequest")
            .field("argv", &self.argv)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field("env", &"[redacted]")
            .field("cwd", &self.cwd)
            .field("stdin", &self.stdin)
            .field("timeout", &self.timeout)
            .field("scope", &self.scope)
            .finish()
    }
}

impl ExecRequest {
    /// A request with the given argv, `/` as cwd and an empty environment.
    pub fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            env: BTreeMap::new(),
            cwd: PathBuf::from("/"),
            stdin: None,
            timeout: None,
            scope: None,
        }
    }

    /// Add one environment variable.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Set the working directory inside the sandbox.
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = cwd.into();
        self
    }

    /// Set the bytes written to standard input.
    pub fn with_stdin(mut self, stdin: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(stdin.into());
        self
    }

    /// Set the wall-clock limit.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Restrict the command to one project scope.
    pub fn with_scope(mut self, scope: ExecScope) -> Self {
        self.scope = Some(scope);
        self
    }

    /// The working directory the command starts in: `/workspace` for a scoped
    /// request that left the default `/`, otherwise the requested one.
    pub fn effective_cwd(&self) -> PathBuf {
        if self.scope.is_some() && self.cwd == Path::new("/") {
            return PathBuf::from(WORKSPACE);
        }
        self.cwd.clone()
    }

    /// The clean environment sent to the command: safe defaults plus the request.
    ///
    /// With an egress port, the HTTP proxy variables point at the in-sandbox
    /// bridge. With a hub socket, `JG_HUB_SOCKET` names the in-sandbox socket.
    /// The request environment still overrides every default.
    pub fn resolved_env(
        &self,
        egress_port: Option<u16>,
        hub_socket: Option<&Path>,
    ) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert("PATH".to_string(), DEFAULT_PATH.to_string());
        let home = if self.scope.is_some() {
            SCOPED_HOME
        } else {
            DEFAULT_HOME
        };
        env.insert("HOME".to_string(), home.to_string());
        env.insert("LANG".to_string(), DEFAULT_LANG.to_string());
        if let Some(port) = egress_port {
            let proxy = format!("http://127.0.0.1:{port}");
            for key in [
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "http_proxy",
                "https_proxy",
                "all_proxy",
            ] {
                env.insert(key.to_string(), proxy.clone());
            }
            env.insert("NO_PROXY".to_string(), "localhost,127.0.0.1".to_string());
        }
        if hub_socket.is_some() {
            env.insert(
                "JG_HUB_SOCKET".to_string(),
                crate::namespace::HUB_SOCKET_PATH.to_string(),
            );
        }
        env.extend(self.env.clone());
        env
    }
}

/// The result of a command run inside a sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    /// Exit code when the command exited normally.
    pub exit_code: Option<i32>,
    /// Terminating signal when the command was killed.
    pub signal: Option<i32>,
    /// Captured standard output.
    pub stdout: Vec<u8>,
    /// Captured standard error.
    pub stderr: Vec<u8>,
    /// Whether the request timeout killed the command.
    pub timed_out: bool,
    /// Whether output was still arriving after the command finished and the
    /// reader stopped after its grace period, so `stdout`/`stderr` may be
    /// incomplete.
    pub truncated: bool,
}

impl ExecOutput {
    /// Standard output as lossy UTF-8, convenient in tests and logs.
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Standard error as lossy UTF-8, convenient in tests and logs.
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::ScopeAccess;

    #[test]
    fn request_env_overrides_defaults() {
        let req = ExecRequest::new(vec!["echo".to_string()])
            .with_env("PATH", "/lab/bin")
            .with_env("CUSTOM", "yes");
        let env = req.resolved_env(None, None);
        assert_eq!(env.get("PATH").map(String::as_str), Some("/lab/bin"));
        assert_eq!(env.get("CUSTOM").map(String::as_str), Some("yes"));
        assert_eq!(env.get("HOME").map(String::as_str), Some(DEFAULT_HOME));
        assert!(!env.contains_key("HTTP_PROXY"));
    }

    #[test]
    fn debug_redacts_environment_values() {
        let value = "secret-value";
        let request = ExecRequest::new(vec!["true".to_string()]).with_env("ALLOWED_SECRET", value);

        let debug = format!("{request:?}");

        assert!(debug.contains("ALLOWED_SECRET"));
        assert!(!debug.contains(value));
    }

    #[test]
    fn egress_port_adds_proxy_env_but_request_wins() {
        let req = ExecRequest::new(vec!["true".to_string()]);
        let env = req.resolved_env(Some(3128), None);
        let proxy = Some("http://127.0.0.1:3128");
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            assert_eq!(env.get(key).map(String::as_str), proxy, "{key}");
        }
        assert_eq!(
            env.get("NO_PROXY").map(String::as_str),
            Some("localhost,127.0.0.1")
        );

        let req = req.with_env("HTTPS_PROXY", "http://request");
        let env = req.resolved_env(Some(3128), None);
        assert_eq!(
            env.get("HTTPS_PROXY").map(String::as_str),
            Some("http://request")
        );
    }

    #[test]
    fn hub_socket_adds_env_but_request_wins() {
        let req = ExecRequest::new(vec!["true".to_string()]);
        let env = req.resolved_env(None, Some(Path::new("/tmp/hub.sock")));
        assert_eq!(
            env.get("JG_HUB_SOCKET").map(String::as_str),
            Some("/run/jg/hub.sock")
        );

        let env = req.resolved_env(None, None);
        assert!(!env.contains_key("JG_HUB_SOCKET"));

        let req = req.with_env("JG_HUB_SOCKET", "/request/hub.sock");
        let env = req.resolved_env(None, Some(Path::new("/tmp/hub.sock")));
        assert_eq!(
            env.get("JG_HUB_SOCKET").map(String::as_str),
            Some("/request/hub.sock")
        );
    }

    #[test]
    fn scoped_request_defaults_to_workspace_cwd_and_tmp_home() {
        let scope = ExecScope::new(ScopeAccess::ReadOnly, "/lab/a");
        let req = ExecRequest::new(vec!["true".to_string()]).with_scope(scope);
        assert_eq!(req.effective_cwd(), Path::new(WORKSPACE));
        let env = req.resolved_env(None, None);
        assert_eq!(env.get("HOME").map(String::as_str), Some(SCOPED_HOME));

        let req = req.with_cwd("/tmp").with_env("HOME", "/tmp/mine");
        assert_eq!(req.effective_cwd(), Path::new("/tmp"));
        let env = req.resolved_env(None, None);
        assert_eq!(env.get("HOME").map(String::as_str), Some("/tmp/mine"));
    }

    #[test]
    fn unscoped_request_keeps_cwd_and_home() {
        let req = ExecRequest::new(vec!["true".to_string()]);
        assert_eq!(req.effective_cwd(), Path::new("/"));
        let env = req.resolved_env(None, None);
        assert_eq!(env.get("HOME").map(String::as_str), Some(DEFAULT_HOME));
    }
}

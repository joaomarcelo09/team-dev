//! A single command run inside a sandbox.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// Default `PATH` given to commands when the request does not override it.
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// Default `HOME` inside the sandbox (inside `/lab`, so it persists).
pub const DEFAULT_HOME: &str = "/lab/home";
/// Default locale used when the request does not override it.
pub const DEFAULT_LANG: &str = "C.UTF-8";

/// A command to run inside a sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
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

    /// The clean environment sent to the command: safe defaults plus the request.
    pub fn resolved_env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert("PATH".to_string(), DEFAULT_PATH.to_string());
        env.insert("HOME".to_string(), DEFAULT_HOME.to_string());
        env.insert("LANG".to_string(), DEFAULT_LANG.to_string());
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

    #[test]
    fn request_env_overrides_defaults() {
        let req = ExecRequest::new(vec!["echo".to_string()])
            .with_env("PATH", "/lab/bin")
            .with_env("CUSTOM", "yes");
        let env = req.resolved_env();
        assert_eq!(env.get("PATH").map(String::as_str), Some("/lab/bin"));
        assert_eq!(env.get("CUSTOM").map(String::as_str), Some("yes"));
        assert_eq!(env.get("HOME").map(String::as_str), Some(DEFAULT_HOME));
    }
}

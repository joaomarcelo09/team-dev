//! Egress allowlist policy: which `host:port` pairs may leave the sandbox.

use std::collections::BTreeSet;

use crate::error::{Result, SandboxError};

/// Ports allowed by [`EgressPolicy::new`] when no custom list is given.
pub const DEFAULT_PORTS: [u16; 2] = [443, 80];

/// The hosts a sandbox may reach by default: package registries and model APIs.
///
/// GitHub is deliberately absent; a sandbox that needs it must be given a
/// policy that names it explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressPolicy {
    hosts: Vec<String>,
    ports: BTreeSet<u16>,
}

impl EgressPolicy {
    /// Build a policy from literal host names or `*.suffix` wildcards, allowing
    /// the [`DEFAULT_PORTS`].
    pub fn new<I, S>(hosts: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::with_ports(hosts, DEFAULT_PORTS)
    }

    /// Build a policy with an explicit set of ports.
    pub fn with_ports<I, S, J>(hosts: I, ports: J) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
        J: IntoIterator<Item = u16>,
    {
        let mut patterns = Vec::new();
        for host in hosts {
            let host = host.into();
            validate_pattern(&host)?;
            patterns.push(host);
        }
        Ok(Self {
            hosts: patterns,
            ports: ports.into_iter().collect(),
        })
    }

    /// The package registries and model APIs the sandbox may reach by default.
    pub fn default_registries() -> Self {
        Self::new([
            // npm
            "registry.npmjs.org",
            // PyPI
            "pypi.org",
            "files.pythonhosted.org",
            // crates.io
            "crates.io",
            "index.crates.io",
            "static.crates.io",
            // model APIs
            "api.anthropic.com",
            "api.openai.com",
            "generativelanguage.googleapis.com",
        ])
        .expect("default registry hosts are valid")
    }

    /// Whether `host` on `port` is allowed.
    pub fn allows(&self, host: &str, port: u16) -> bool {
        self.allows_port(port) && self.allows_host(host)
    }

    /// Whether `host` is named by an exact entry or a `*.suffix` wildcard.
    pub fn allows_host(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.hosts
            .iter()
            .any(|pattern| host_matches(pattern, &host))
    }

    /// Whether `port` is in the policy's port set.
    pub fn allows_port(&self, port: u16) -> bool {
        self.ports.contains(&port)
    }

    /// The configured host patterns, in the order they were given.
    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }

    /// The configured ports, sorted.
    pub fn ports(&self) -> &BTreeSet<u16> {
        &self.ports
    }
}

/// Whether a CONNECT host name is syntactically valid.
///
/// Hosts are ASCII labels joined by dots; each label is 1-63 characters of
/// letters, digits and hyphens and does not start or end with a hyphen.
pub(crate) fn is_valid_hostname(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && label
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

fn validate_pattern(pattern: &str) -> Result<()> {
    if pattern.is_empty() {
        return Err(SandboxError::invalid("egress host pattern is empty"));
    }
    let host = match pattern.strip_prefix("*.") {
        Some(suffix) => suffix,
        None => {
            if pattern.contains('*') {
                return Err(SandboxError::invalid(format!(
                    "egress wildcard must be a leading '*.' in {pattern}"
                )));
            }
            pattern
        }
    };
    if pattern != pattern.to_ascii_lowercase() {
        return Err(SandboxError::invalid(format!(
            "egress host must be lowercase: {pattern}"
        )));
    }
    if pattern.contains("://") || pattern.contains('/') || pattern.contains(':') {
        return Err(SandboxError::invalid(format!(
            "egress host must not include a scheme, port or path: {pattern}"
        )));
    }
    if !is_valid_hostname(host) {
        return Err(SandboxError::invalid(format!(
            "invalid egress hostname: {pattern}"
        )));
    }
    Ok(())
}

/// Match one policy pattern against a lowercase host.
///
/// `*.example.com` matches `a.example.com` and `a.b.example.com` but never the
/// bare `example.com` or a lookalike such as `evil-example.com`.
fn host_matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(suffix) => match host.strip_suffix(suffix) {
            Some(prefix) => prefix.len() > 1 && prefix.ends_with('.'),
            None => false,
        },
        None => host == pattern,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(hosts: &[&str]) -> EgressPolicy {
        EgressPolicy::new(hosts.iter().copied()).expect("valid policy")
    }

    #[test]
    fn exact_hosts_match_only_themselves() {
        let policy = policy(&["registry.npmjs.org"]);

        assert!(policy.allows("registry.npmjs.org", 443));
        assert!(!policy.allows("evilnpmjs.org", 443));
        assert!(!policy.allows("registry.npmjs.org.evil.com", 443));
        assert!(!policy.allows("registry.npmjs.org", 8443));
    }

    #[test]
    fn wildcards_match_real_subdomains_only() {
        let policy = policy(&["*.example.com"]);

        assert!(policy.allows("a.example.com", 443));
        assert!(policy.allows("a.b.example.com", 443));
        assert!(!policy.allows("example.com", 443));
        assert!(!policy.allows("evil-example.com", 443));
        assert!(!policy.allows("example.com.evil.com", 443));
        assert!(!policy.allows(".example.com", 443));
    }

    #[test]
    fn default_ports_are_https_and_http() {
        let policy = policy(&["registry.npmjs.org"]);

        assert!(policy.allows_port(443));
        assert!(policy.allows_port(80));
        assert!(!policy.allows_port(22));
        assert_eq!(policy.ports().len(), 2);
    }

    #[test]
    fn custom_ports_replace_the_defaults() {
        let policy =
            EgressPolicy::with_ports(["registry.npmjs.org"], [8443]).expect("valid policy");

        assert!(policy.allows("registry.npmjs.org", 8443));
        assert!(!policy.allows("registry.npmjs.org", 443));
    }

    #[test]
    fn default_registries_cover_packages_and_model_apis() {
        let policy = EgressPolicy::default_registries();

        for host in [
            "registry.npmjs.org",
            "pypi.org",
            "files.pythonhosted.org",
            "crates.io",
            "index.crates.io",
            "static.crates.io",
            "api.anthropic.com",
            "api.openai.com",
            "generativelanguage.googleapis.com",
        ] {
            assert!(policy.allows(host, 443), "{host} should be allowed");
        }
        assert!(!policy.allows("github.com", 443));
        assert!(!policy.allows("api.github.com", 443));
    }

    #[test]
    fn invalid_patterns_are_rejected() {
        for pattern in [
            "",
            "Registry.npmjs.org",
            "https://registry.npmjs.org",
            "registry.npmjs.org:443",
            "registry.npmjs.org/path",
            "*",
            "*.",
            "foo.*",
            "exa_mple.com",
            "-example.com",
            "example-.com",
            "example..com",
            "example.com.",
        ] {
            assert!(
                EgressPolicy::new([pattern]).is_err(),
                "{pattern:?} should be rejected"
            );
        }
    }

    #[test]
    fn single_label_hosts_are_valid_patterns() {
        let policy = policy(&["registry"]);

        assert!(policy.allows("registry", 443));
        assert!(!policy.allows("registry.example.com", 443));
    }
}

//! Visibility, read-only and network isolation tests.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::net::UnixListener;
use std::sync::Mutex;

use jg_sandbox::{NamespaceBackend, SandboxSpec};
use support::{create_or_skip, fixture, run_ok, run_shell};

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvRestore {
    home: Option<std::ffi::OsString>,
    secret: Option<std::ffi::OsString>,
}

impl EnvRestore {
    fn capture() -> Self {
        Self {
            home: std::env::var_os("HOME"),
            secret: std::env::var_os("JGF_TEST_SECRET"),
        }
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        // SAFETY: ENV_LOCK serialises all environment changes in this test binary.
        unsafe {
            match &self.home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match &self.secret {
                Some(value) => std::env::set_var("JGF_TEST_SECRET", value),
                None => std::env::remove_var("JGF_TEST_SECRET"),
            }
        }
    }
}

#[test]
fn sandbox_is_visibly_isolated() {
    let _lock = ENV_LOCK.lock().expect("env lock");
    let restore = EnvRestore::capture();

    let lab = fixture();
    let fake_home = lab.temp.path().join("host-home");
    std::fs::create_dir_all(fake_home.join(".ssh")).expect("fake ssh dir");
    std::fs::create_dir_all(fake_home.join(".config")).expect("fake config dir");
    std::fs::create_dir_all(fake_home.join(".claude")).expect("fake claude dir");
    std::fs::write(fake_home.join(".gh-token"), "fake-token").expect("fake token");

    // SAFETY: ENV_LOCK serialises all environment changes in this test binary.
    unsafe {
        std::env::set_var("HOME", &fake_home);
        std::env::set_var("JGF_TEST_SECRET", "host-secret-value");
    }

    let socket_path = lab.temp.path().join("hub.sock");
    let _listener = UnixListener::bind(&socket_path).expect("bind socket");

    let mut spec = lab.spec.clone();
    spec.allowed_sockets = vec![socket_path.clone()];

    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &spec) else {
        return;
    };
    let handle = guard.handle();

    let script = concat!(
        "echo \"SECRET=${JGF_TEST_SECRET-}\"\n",
        "echo \"HOME=$HOME\"\n",
        "find / -maxdepth 4 \\( -path /proc -o -path /usr -o -path /dev \\) -prune -o -print 2>/dev/null\n",
        "echo '---SOCKET-FILES---'\n",
        "find / -maxdepth 6 \\( -path /proc -o -path /usr -o -path /dev \\) -prune -o -exec stat -c '%F %n' {} + 2>/dev/null | sed -n 's/^socket //p'\n",
        "echo '---UNIX-TABLE---'\n",
        "cat /proc/net/unix\n",
    );
    let output = run_ok(backend, handle, script);
    let text = output.stdout_text();
    drop(restore);
    drop(_lock);

    let mut sections = text.split("---SOCKET-FILES---");
    let listing = sections.next().expect("listing section");
    let rest = sections.next().expect("socket section");
    let mut socket_sections = rest.split("---UNIX-TABLE---");
    let socket_files = socket_sections.next().expect("socket files");
    let unix_table = socket_sections.next().expect("unix table");

    let lines: Vec<&str> = listing.lines().collect();
    assert!(
        lines.contains(&"SECRET="),
        "host secret leaked into the sandbox: {listing}"
    );
    assert!(
        lines.contains(&"HOME=/lab/home"),
        "HOME is not the sandbox home: {listing}"
    );

    let fake_home_text = fake_home.to_string_lossy().into_owned();
    for line in &lines {
        let path = line.trim();
        if !path.starts_with('/') {
            continue;
        }
        assert!(!path.starts_with("/home"), "host /home is visible: {path}");
        assert!(!path.starts_with("/root"), "host /root is visible: {path}");
        assert!(!path.starts_with("/run"), "host /run is visible: {path}");
        assert!(
            !path.starts_with(fake_home_text.as_str()),
            "host home is visible: {path}"
        );
        assert!(!path.contains("/.ssh"), "host .ssh is visible: {path}");
        assert!(
            !path.contains("/.config"),
            "host .config is visible: {path}"
        );
        assert!(
            !path.contains("/.claude"),
            "host .claude is visible: {path}"
        );
        assert!(
            !path.contains(".gh-token"),
            "host token marker is visible: {path}"
        );
    }

    let found: BTreeSet<String> = socket_files
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('/'))
        .map(str::to_string)
        .collect();
    let expected: BTreeSet<String> = [socket_path.to_string_lossy().into_owned()]
        .into_iter()
        .collect();
    assert_eq!(found, expected, "unexpected unix sockets inside");

    let table_paths: BTreeSet<String> = unix_table
        .lines()
        .filter_map(|line| line.split_whitespace().last())
        .filter(|field| field.starts_with('/'))
        .map(str::to_string)
        .collect();
    assert!(
        table_paths.is_subset(&expected),
        "unexpected sockets in /proc/net/unix: {table_paths:?}"
    );
}

#[test]
fn read_only_mounts_and_pivot_hide_the_host() {
    let lab = fixture();
    let lab_dir = lab.spec.lab_dir.clone();
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();

    let write_usr = run_shell(backend, handle, "echo nope > /usr/forbidden.txt");
    assert_ne!(
        write_usr.exit_code,
        Some(0),
        "/usr must be read-only: {}",
        write_usr.stderr_text()
    );

    let write_lab = run_ok(
        backend,
        handle,
        "echo lab-data > /lab/write.txt && cat /lab/write.txt",
    );
    assert_eq!(write_lab.stdout_text(), "lab-data\n");
    assert!(
        lab_dir.join("write.txt").exists(),
        "lab directory is not writable from the host"
    );

    let pivot = run_ok(
        backend,
        handle,
        "test -e /proc/1/root/home && echo leak || echo no-home",
    );
    assert_eq!(pivot.stdout_text(), "no-home\n");

    let dotdot = run_ok(
        backend,
        handle,
        "cd /../../.. && pwd && test -e /home && echo leak || echo no-home",
    );
    let text = dotdot.stdout_text();
    assert!(
        text.starts_with("/\n"),
        "dotdot walk escaped the root: {text}"
    );
    assert!(text.contains("no-home"), "host /home reachable via dotdot");

    let passwd = run_ok(backend, handle, "cat /proc/1/root/etc/passwd");
    assert_eq!(
        passwd.stdout_text().lines().count(),
        2,
        "host /etc/passwd is reachable through /proc/1/root"
    );
    assert!(
        passwd
            .stdout_text()
            .starts_with("root:x:0:0:root:/lab/home:")
    );

    run_ok(backend, handle, "test -L /bin && test -x /bin/sh");
}

#[test]
fn command_cannot_regain_privileges() {
    let lab = fixture();
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();

    let script = concat!(
        "grep -E '^(NoNewPrivs|CapEff|CapPrm|CapInh|CapBnd):' /proc/self/status\n",
        "if command -v mount >/dev/null 2>&1; then echo MOUNT-PRESENT; else echo MOUNT-MISSING; fi\n",
        "if mount -o remount,rw,bind /usr >/dev/null 2>&1; then\n",
        "  echo REMOUNT-ALLOWED\n",
        "else\n",
        "  echo REMOUNT-DENIED\n",
        "fi\n",
        "awk '$2 == \"/usr\" { print \"MOUNT\", $4 }' /proc/mounts\n",
    );
    let output = run_ok(backend, handle, script);
    let text = output.stdout_text();

    let mut status = BTreeMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(':')
            && matches!(
                key,
                "NoNewPrivs" | "CapEff" | "CapPrm" | "CapInh" | "CapBnd"
            )
        {
            status.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    assert_eq!(
        status.get("NoNewPrivs").map(String::as_str),
        Some("1"),
        "no_new_privs is not set: {text}"
    );
    for key in ["CapEff", "CapPrm", "CapInh", "CapBnd"] {
        let value = status
            .get(key)
            .unwrap_or_else(|| panic!("{key} missing: {text}"));
        let bits = u64::from_str_radix(value, 16).expect("hex capability mask");
        assert_eq!(bits, 0, "{key} is not empty: {value}");
    }

    assert!(
        text.lines().any(|line| line == "MOUNT-PRESENT"),
        "mount is not available inside the sandbox: {text}"
    );
    assert!(
        text.lines().any(|line| line == "REMOUNT-DENIED"),
        "a command remounted /usr read-write: {text}"
    );
    let usr_options: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix("MOUNT "))
        .collect();
    assert!(!usr_options.is_empty(), "no /usr mount options: {text}");
    for options in usr_options {
        assert!(
            options.split(',').any(|option| option == "ro"),
            "/usr is writable after the failed remount: {options}"
        );
    }
}

#[test]
fn read_only_binds_cover_every_submount() {
    let lab = fixture();
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();

    let script = concat!(
        "awk '$5 == \"/usr\" || index($5, \"/usr/\") == 1 { print $5, $6 }' ",
        "/proc/self/mountinfo\n",
    );
    let output = run_ok(backend, handle, script);
    let text = output.stdout_text();
    let mut saw_usr = false;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let mount_point = parts.next().expect("mount point");
        let options = parts.next().expect("mount options");
        if mount_point == "/usr" {
            saw_usr = true;
        }
        assert!(
            options.split(',').any(|option| option == "ro"),
            "mount {mount_point} below a read-only bind is writable: {options}"
        );
    }
    assert!(saw_usr, "/usr is not mounted read-only: {text}");

    run_ok(backend, handle, "touch /tmp/writable && echo tmp-ok");
}

#[test]
fn network_namespace_has_only_loopback() {
    let lab = fixture();
    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &lab.spec) else {
        return;
    };
    let handle = guard.handle();

    let output = run_ok(backend, handle, "cat /proc/net/dev");
    let table = output.stdout_text();
    let interfaces: Vec<&str> = table
        .lines()
        .filter(|line| line.contains(':'))
        .map(|line| line.split(':').next().unwrap_or("").trim())
        .filter(|name| !name.is_empty())
        .collect();
    assert_eq!(interfaces, vec!["lo"], "unexpected network interfaces");
}

#[test]
fn allowed_socket_is_the_only_socket_exposed() {
    let lab = fixture();
    let socket_path = lab.temp.path().join("broker.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind socket");
    let mut spec = SandboxSpec::new(lab.spec.state_dir.clone(), lab.spec.lab_dir.clone());
    spec.allowed_sockets = vec![socket_path.clone()];

    let backend = NamespaceBackend::new();
    let Some(guard) = create_or_skip(backend, &spec) else {
        return;
    };
    let handle = guard.handle();

    let script = concat!(
        "find / -maxdepth 6 \\( -path /proc -o -path /usr -o -path /dev \\) -prune ",
        "-o -exec stat -c '%F %n' {} + 2>/dev/null | sed -n 's/^socket //p'",
    );
    let output = run_ok(backend, handle, script);
    let found: BTreeSet<String> = output
        .stdout_text()
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('/'))
        .map(str::to_string)
        .collect();
    let expected: BTreeSet<String> = [socket_path.to_string_lossy().into_owned()]
        .into_iter()
        .collect();
    assert_eq!(found, expected);
    drop(listener);
}

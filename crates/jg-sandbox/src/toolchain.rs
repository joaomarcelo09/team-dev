//! Discovering the host toolchains a sandbox may build with.
//!
//! A sandbox sees only `/usr`, `/etc/ssl/certs` and the lab unless the caller
//! exposes more host directories read-only. [`Toolchain::discover`] looks for
//! the Node.js, Python, Rust and Tauri build tools on the host, and
//! [`Toolchain::ro_binds`] returns the directories to bind read-only so the
//! sandbox can run them without its own copy of the host toolchain.
//!
//! Only toolchain roots are exposed. Files that carry credentials or per-user
//! state (`~/.npmrc`, `~/.cargo/credentials*`, `~/.config`) are never bound:
//! the cargo home is exposed as its `bin` directory only, and any other
//! installation prefix under the home (such as `~/.local` or an npm prefix) is
//! narrowed to the binary's own directory. The caches a build writes are
//! pointed at the scope's private home by [`Toolchain::env`], never at the
//! host's.
//!
//! Package downloads are allowed by [`crate::EgressPolicy::default_registries`],
//! which already covers npm, PyPI and crates.io (including `index.crates.io`
//! and `static.crates.io`) and deliberately excludes GitHub.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use crate::error::{Result, SandboxError};
use crate::exec::DEFAULT_PATH;

/// Directories searched for executables when `PATH` does not name one.
const SYSTEM_BIN_DIRS: [&str; 6] = [
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/local/sbin",
    "/usr/sbin",
    "/sbin",
];

/// Directories searched for `pkg-config` metadata, besides multiarch ones.
const SYSTEM_PKGCONFIG_DIRS: [&str; 6] = [
    "/usr/local/lib/pkgconfig",
    "/usr/local/lib64/pkgconfig",
    "/usr/local/share/pkgconfig",
    "/usr/lib/pkgconfig",
    "/usr/lib64/pkgconfig",
    "/usr/share/pkgconfig",
];

/// The `pkg-config` file of GTK 3.
const GTK3_PC: &str = "gtk+-3.0.pc";
/// The `pkg-config` files of WebKitGTK 4.1 and 4.0.
const WEBKIT2GTK_PC: [&str; 2] = ["webkit2gtk-4.1.pc", "webkit2gtk-4.0.pc"];
/// The `pkg-config` files of libsoup 3.0 and 2.4.
const LIBSOUP_PC: [&str; 2] = ["libsoup-3.0.pc", "libsoup-2.4.pc"];

/// A JavaScript package manager found on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManagerKind {
    /// npm, which uses the npm registry.
    Npm,
    /// pnpm, which also uses the npm registry.
    Pnpm,
}

impl PackageManagerKind {
    /// The executable name of this package manager.
    pub fn binary_name(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
        }
    }
}

/// The JavaScript package manager a build should use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageManager {
    /// Which manager it is.
    pub kind: PackageManagerKind,
    /// Absolute path of its binary.
    pub binary: PathBuf,
}

/// Tauri's native build dependencies and their `pkg-config` metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TauriLibs {
    /// The `pkg-config` binary, when present.
    pub pkg_config: Option<PathBuf>,
    /// Whether the GTK 3 development files are present.
    pub gtk3: bool,
    /// Whether the WebKitGTK development files are present.
    pub webkit2gtk: bool,
    /// Whether the libsoup development files are present.
    pub libsoup: bool,
    /// Host directories holding at least one of the Tauri `pkg-config` files.
    pub pkgconfig_dirs: Vec<PathBuf>,
}

impl TauriLibs {
    /// Whether every Tauri build dependency was found.
    pub fn is_complete(&self) -> bool {
        self.pkg_config.is_some() && self.gtk3 && self.webkit2gtk && self.libsoup
    }
}

/// The host toolchains available to a sandbox.
///
/// Every path is a host absolute path. A missing field means that tool is not
/// installed on the host; the caller decides whether that is an error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Toolchain {
    /// The `node` binary.
    pub node: Option<PathBuf>,
    /// The package manager to use for a Node project, preferring pnpm.
    pub package_manager: Option<PackageManager>,
    /// The `python3` binary, falling back to `python`.
    pub python: Option<PathBuf>,
    /// The `uv` binary.
    pub uv: Option<PathBuf>,
    /// The `cargo` binary.
    pub cargo: Option<PathBuf>,
    /// The `rustup` binary.
    pub rustup: Option<PathBuf>,
    /// Host rustup installation holding the toolchains, when rustup is present.
    ///
    /// It is exposed read-only; rustup is not expected to install components
    /// inside a sandbox.
    pub rustup_home: Option<PathBuf>,
    /// Tauri's native build dependencies.
    pub tauri: TauriLibs,
}

impl Toolchain {
    /// Discover the host toolchains using the host `PATH`, `HOME`,
    /// `CARGO_HOME`, `RUSTUP_HOME` and `PKG_CONFIG_PATH`.
    pub fn discover() -> Self {
        let mut toolchain = Self::discover_in(&host_bin_dirs(), &host_pkgconfig_dirs());
        if toolchain.rustup.is_some() {
            toolchain.rustup_home = host_rustup_home();
        }
        toolchain
    }

    /// The host directories to mount read-only: the default binds plus every
    /// existing toolchain root, without duplicates.
    ///
    /// A root already covered by an earlier bind is left out, so the result
    /// can replace [`crate::SandboxSpec::ro_binds`] directly.
    pub fn ro_binds(&self) -> Vec<PathBuf> {
        let home = host_home();
        let cargo_home = host_cargo_home(home.as_deref());
        self.ro_binds_for(home.as_deref(), cargo_home.as_deref())
    }

    /// Environment variables for a build in a scope whose `HOME` is `home`.
    ///
    /// `PATH` starts with the discovered tool directories and keeps the
    /// sandbox defaults; `CARGO_HOME`, the npm cache and the uv cache live
    /// under `home`, so a build never writes a host cache. `RUSTUP_HOME` names
    /// the read-only host rustup installation when there is one. The caller
    /// applies these through [`crate::ExecRequest::with_env`], which lets a
    /// request override them.
    pub fn env(&self, home: &Path) -> Result<BTreeMap<String, String>> {
        if !home.is_absolute() || home.components().any(|part| part == Component::ParentDir) {
            return Err(SandboxError::invalid(format!(
                "toolchain home {} must be absolute and without ..",
                home.display()
            )));
        }
        let mut env = BTreeMap::new();
        env.insert("PATH".to_string(), self.path_value());
        env.insert(
            "CARGO_HOME".to_string(),
            home.join(".cargo").display().to_string(),
        );
        env.insert(
            "npm_config_cache".to_string(),
            home.join(".npm").display().to_string(),
        );
        env.insert(
            "UV_CACHE_DIR".to_string(),
            home.join(".cache/uv").display().to_string(),
        );
        if let Some(rustup_home) = &self.rustup_home {
            env.insert("RUSTUP_HOME".to_string(), rustup_home.display().to_string());
        }
        Ok(env)
    }

    /// Discover toolchains, looking for binaries and `pkg-config` files only in
    /// the given directories.
    fn discover_in(bin_dirs: &[PathBuf], pkgconfig_dirs: &[PathBuf]) -> Self {
        let bin_dirs = dedup_paths(bin_dirs.iter().cloned());
        let package_manager = find_package_manager(&bin_dirs);
        let python = find_binary(&bin_dirs, "python3").or_else(|| find_binary(&bin_dirs, "python"));
        let rustup = find_binary(&bin_dirs, "rustup");
        Self {
            node: find_binary(&bin_dirs, "node"),
            package_manager,
            python,
            uv: find_binary(&bin_dirs, "uv"),
            cargo: find_binary(&bin_dirs, "cargo"),
            rustup,
            rustup_home: None,
            tauri: discover_tauri(&bin_dirs, pkgconfig_dirs),
        }
    }

    /// Like [`Self::ro_binds`], with the host home and cargo home made explicit.
    fn ro_binds_for(&self, home: Option<&Path>, cargo_home: Option<&Path>) -> Vec<PathBuf> {
        let mut binds = crate::spec::default_ro_binds();
        let mut roots = self.tool_roots(home);
        // Parents before children, so a broad root wins over a redundant bind.
        roots.sort_by_key(|path| path.components().count());
        for candidate in roots {
            let Some(root) = sanitize_tool_root(&candidate, home, cargo_home) else {
                continue;
            };
            if !root.is_dir() || binds.iter().any(|bind| root.starts_with(bind)) {
                continue;
            }
            binds.push(root);
        }
        binds
    }

    /// Every host path that belongs to a discovered toolchain.
    fn tool_roots(&self, home: Option<&Path>) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = self
            .binaries()
            .into_iter()
            .map(|binary| tool_root(binary, home))
            .collect();
        if let Some(rustup_home) = &self.rustup_home {
            roots.push(rustup_home.clone());
        }
        roots.extend(self.tauri.pkgconfig_dirs.iter().cloned());
        roots
    }

    /// The binaries of every discovered tool.
    fn binaries(&self) -> Vec<&Path> {
        [
            self.node.as_deref(),
            self.package_manager
                .as_ref()
                .map(|manager| manager.binary.as_path()),
            self.python.as_deref(),
            self.uv.as_deref(),
            self.cargo.as_deref(),
            self.rustup.as_deref(),
            self.tauri.pkg_config.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// The `PATH` value: tool directories first, then the sandbox defaults.
    fn path_value(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for binary in self.binaries() {
            if let Some(dir) = binary.parent() {
                push_unique(&mut parts, dir.display().to_string());
            }
        }
        for dir in DEFAULT_PATH.split(':') {
            push_unique(&mut parts, dir.to_string());
        }
        parts.join(":")
    }
}

fn push_unique(parts: &mut Vec<String>, value: String) {
    if !parts.contains(&value) {
        parts.push(value);
    }
}

fn dedup_paths(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut unique = Vec::new();
    for path in paths {
        if !unique.contains(&path) {
            unique.push(path);
        }
    }
    unique
}

fn find_binary(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    dirs.iter()
        .map(|dir| dir.join(name))
        .find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

fn find_package_manager(dirs: &[PathBuf]) -> Option<PackageManager> {
    for kind in [PackageManagerKind::Pnpm, PackageManagerKind::Npm] {
        if let Some(binary) = find_binary(dirs, kind.binary_name()) {
            return Some(PackageManager { kind, binary });
        }
    }
    None
}

fn discover_tauri(bin_dirs: &[PathBuf], pkgconfig_dirs: &[PathBuf]) -> TauriLibs {
    let mut tauri = TauriLibs {
        pkg_config: find_binary(bin_dirs, "pkg-config")
            .or_else(|| find_binary(bin_dirs, "pkgconf")),
        ..TauriLibs::default()
    };
    for dir in dedup_paths(pkgconfig_dirs.iter().cloned()) {
        if !dir.is_dir() {
            continue;
        }
        let gtk3 = has_pkgconfig_file(&dir, &[GTK3_PC]);
        let webkit2gtk = has_pkgconfig_file(&dir, &WEBKIT2GTK_PC);
        let libsoup = has_pkgconfig_file(&dir, &LIBSOUP_PC);
        tauri.gtk3 |= gtk3;
        tauri.webkit2gtk |= webkit2gtk;
        tauri.libsoup |= libsoup;
        if gtk3 || webkit2gtk || libsoup {
            tauri.pkgconfig_dirs.push(dir);
        }
    }
    tauri
}

fn has_pkgconfig_file(dir: &Path, names: &[&str]) -> bool {
    names.iter().any(|name| dir.join(name).is_file())
}

/// The host directory to bind for a binary.
///
/// It is the installation prefix (the parent of its `bin` directory), except
/// that a prefix under `$HOME` which is not a known toolchain root is narrowed
/// to the binary's own directory. That keeps generic prefixes such as `~/.local`
/// (for `~/.local/bin/uv`) or an npm global prefix from exposing unrelated
/// per-user files.
fn tool_root(binary: &Path, home: Option<&Path>) -> PathBuf {
    let own_dir = binary.parent().unwrap_or(Path::new("/"));
    let prefix = install_prefix(own_dir);
    match home {
        Some(home) if prefix.starts_with(home) && !is_known_home_toolchain_root(&prefix, home) => {
            own_dir.to_path_buf()
        }
        _ => prefix,
    }
}

/// The installation prefix of a binary directory: its parent when it is a `bin`
/// directory (`~/.cargo/bin` belongs to `~/.cargo`), else the directory itself.
fn install_prefix(own_dir: &Path) -> PathBuf {
    if own_dir.file_name().and_then(|name| name.to_str()) != Some("bin") {
        return own_dir.to_path_buf();
    }
    own_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| own_dir.to_path_buf())
}

/// Whether `root` is a toolchain root that may be exposed whole even when it
/// lies under the home directory.
fn is_known_home_toolchain_root(root: &Path, home: &Path) -> bool {
    if root == home.join(".rustup") {
        return true;
    }
    // nvm keeps a complete Node installation in each version directory.
    let Ok(relative) = root.strip_prefix(home) else {
        return false;
    };
    let parts: Vec<Component> = relative.components().collect();
    matches!(
        parts.as_slice(),
        [Component::Normal(nvm), Component::Normal(versions), Component::Normal(node), Component::Normal(_)]
            if nvm.to_str() == Some(".nvm")
                && versions.to_str() == Some("versions")
                && node.to_str() == Some("node")
    )
}

/// Home directories that are never exposed, not even for a binary that lives
/// directly in them: they hold per-user state and secrets rather than a
/// toolchain.
const GENERIC_HOME_DIRS: [&str; 5] = [".local", ".cache", ".config", ".npm", ".npm-global"];

/// Whether a root under the home directory may be bound.
///
/// Known toolchain roots and a binary's own `bin` directory are safe. A generic
/// per-user prefix (`~/.local`, an npm, cache or config directory and anything
/// inside them) is not: it holds unrelated state and secrets.
fn is_bindable_home_root(root: &Path, home: &Path) -> bool {
    if is_known_home_toolchain_root(root, home) {
        return true;
    }
    if root.file_name().and_then(|name| name.to_str()) == Some("bin") {
        return true;
    }
    !GENERIC_HOME_DIRS
        .iter()
        .any(|dir| root.starts_with(home.join(dir)))
}

/// Drop a host path that must not be exposed inside a sandbox.
///
/// The home directory, its ancestors and `~/.config` are never bound, and only
/// known toolchain roots or a binary's own directory may come from under the
/// home. The cargo home holds registry credentials, so only its `bin` directory
/// is exposed instead of the whole directory.
fn sanitize_tool_root(
    root: &Path,
    home: Option<&Path>,
    cargo_home: Option<&Path>,
) -> Option<PathBuf> {
    if root == Path::new("/") {
        return None;
    }
    if let Some(home) = home {
        if root == home || home.starts_with(root) || root == home.join(".config") {
            return None;
        }
        // The host cargo home and `~/.cargo` hold credentials; expose only the
        // `bin` directory either way.
        if cargo_home == Some(root) || root == home.join(".cargo") {
            return Some(root.join("bin"));
        }
        if root.starts_with(home) && !is_bindable_home_root(root, home) {
            return None;
        }
    }
    Some(root.to_path_buf())
}

fn host_home() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    home.is_absolute().then_some(home)
}

fn host_cargo_home(home: Option<&Path>) -> Option<PathBuf> {
    if let Some(cargo_home) = std::env::var_os("CARGO_HOME") {
        let cargo_home = PathBuf::from(cargo_home);
        if cargo_home.is_absolute() {
            return Some(cargo_home);
        }
    }
    home.map(|home| home.join(".cargo"))
}

fn host_rustup_home() -> Option<PathBuf> {
    let rustup_home = match std::env::var_os("RUSTUP_HOME") {
        Some(rustup_home) => PathBuf::from(rustup_home),
        None => host_home()?.join(".rustup"),
    };
    rustup_home.is_dir().then_some(rustup_home)
}

/// Directories that may hold a host executable.
fn host_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path).filter(|dir| dir.is_absolute()));
    }
    dirs.extend(SYSTEM_BIN_DIRS.iter().map(PathBuf::from));
    if let Some(home) = host_home() {
        if let Some(cargo_home) = host_cargo_home(Some(&home)) {
            dirs.push(cargo_home.join("bin"));
        }
        dirs.push(home.join(".cargo/bin"));
        dirs.push(home.join(".local/bin"));
        dirs.extend(nvm_bin_dirs(&home));
    }
    dedup_paths(dirs)
}

/// The `bin` directories of every nvm-installed Node, newest first.
fn nvm_bin_dirs(home: &Path) -> Vec<PathBuf> {
    let versions_dir = home.join(".nvm/versions/node");
    let Ok(entries) = std::fs::read_dir(&versions_dir) else {
        return Vec::new();
    };
    let mut versions: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.join("bin").is_dir())
        .collect();
    versions.sort_by_key(|path| std::cmp::Reverse(version_key(path)));
    versions
        .into_iter()
        .map(|version| version.join("bin"))
        .collect()
}

/// Numeric components of a version directory name (`v20.11.0` -> `[20, 11, 0]`).
fn version_key(path: &Path) -> Vec<u64> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            name.split(|c: char| !c.is_ascii_digit())
                .filter(|part| !part.is_empty())
                .filter_map(|part| part.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Directories that may hold `pkg-config` metadata.
fn host_pkgconfig_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(path) = std::env::var_os("PKG_CONFIG_PATH") {
        dirs.extend(std::env::split_paths(&path).filter(|dir| dir.is_absolute()));
    }
    dirs.extend(SYSTEM_PKGCONFIG_DIRS.iter().map(PathBuf::from));
    // Multiarch distributions keep their metadata in
    // `/usr/lib/<triplet>/pkgconfig`, `/lib/<triplet>/pkgconfig` and the like.
    for lib in ["/usr/lib", "/usr/local/lib", "/lib", "/lib64"] {
        let Ok(entries) = std::fs::read_dir(lib) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path().join("pkgconfig");
            if dir.is_dir() {
                dirs.push(dir);
            }
        }
    }
    dedup_paths(dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_executable(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("bin dir");
        let path = dir.join(name);
        std::fs::write(&path, b"#!/bin/sh\n").expect("write executable");
        let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("permissions");
        path
    }

    #[test]
    fn discovery_finds_all_tools_and_prefers_pnpm() {
        let temp = tempfile::tempdir().expect("temp dir");
        let bin = temp.path().join("bin");
        for name in [
            "node",
            "pnpm",
            "python3",
            "uv",
            "cargo",
            "rustup",
            "pkg-config",
        ] {
            write_executable(&bin, name);
        }
        let pkgconfig = temp.path().join("lib/pkgconfig");
        std::fs::create_dir_all(&pkgconfig).expect("pkgconfig dir");
        for name in ["gtk+-3.0.pc", "webkit2gtk-4.1.pc", "libsoup-3.0.pc"] {
            std::fs::write(pkgconfig.join(name), b"").expect("write pc file");
        }

        let toolchain =
            Toolchain::discover_in(std::slice::from_ref(&bin), std::slice::from_ref(&pkgconfig));

        assert_eq!(toolchain.node, Some(bin.join("node")));
        let manager = toolchain.package_manager.expect("pnpm is preferred");
        assert_eq!(manager.kind, PackageManagerKind::Pnpm);
        assert_eq!(manager.binary, bin.join("pnpm"));
        assert_eq!(toolchain.python, Some(bin.join("python3")));
        assert_eq!(toolchain.uv, Some(bin.join("uv")));
        assert_eq!(toolchain.cargo, Some(bin.join("cargo")));
        assert_eq!(toolchain.rustup, Some(bin.join("rustup")));
        assert_eq!(toolchain.tauri.pkg_config, Some(bin.join("pkg-config")));
        assert!(toolchain.tauri.gtk3);
        assert!(toolchain.tauri.webkit2gtk);
        assert!(toolchain.tauri.libsoup);
        assert!(toolchain.tauri.is_complete());
        assert_eq!(toolchain.tauri.pkgconfig_dirs, vec![pkgconfig]);
    }

    #[test]
    fn discovery_falls_back_to_npm_and_python() {
        let temp = tempfile::tempdir().expect("temp dir");
        let bin = temp.path().join("bin");
        write_executable(&bin, "node");
        write_executable(&bin, "npm");
        write_executable(&bin, "python");

        let toolchain = Toolchain::discover_in(std::slice::from_ref(&bin), &[]);

        let manager = toolchain.package_manager.expect("npm");
        assert_eq!(manager.kind, PackageManagerKind::Npm);
        assert_eq!(manager.binary, bin.join("npm"));
        assert_eq!(toolchain.python, Some(bin.join("python")));
        assert_eq!(toolchain.tauri, TauriLibs::default());
    }

    #[test]
    fn non_executable_files_are_not_toolchains() {
        let temp = tempfile::tempdir().expect("temp dir");
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("bin dir");
        std::fs::write(bin.join("node"), b"not executable").expect("write file");

        let toolchain = Toolchain::discover_in(&[bin], &[]);
        assert_eq!(toolchain.node, None);
    }

    #[test]
    fn incomplete_tauri_libs_are_reported() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pkgconfig = temp.path().join("pkgconfig");
        std::fs::create_dir_all(&pkgconfig).expect("pkgconfig dir");
        std::fs::write(pkgconfig.join("gtk+-3.0.pc"), b"").expect("write pc file");

        let tauri = discover_tauri(&[], std::slice::from_ref(&pkgconfig));
        assert!(tauri.gtk3);
        assert!(!tauri.webkit2gtk);
        assert!(!tauri.libsoup);
        assert!(!tauri.is_complete());
        assert_eq!(tauri.pkgconfig_dirs, vec![pkgconfig]);
    }

    #[test]
    fn binds_expose_tool_roots_but_never_the_home_or_cargo_credentials() {
        let temp = tempfile::tempdir().expect("temp dir");
        let home = temp.path().join("home");
        let cargo_home = home.join(".cargo");
        let cargo_bin = cargo_home.join("bin");
        let cargo = write_executable(&cargo_bin, "cargo");
        write_executable(&cargo_bin, "rustup");
        let rustup = cargo_bin.join("rustup");
        std::fs::write(cargo_home.join("credentials.toml"), b"token").expect("credentials");
        let rustup_home = home.join(".rustup");
        std::fs::create_dir_all(rustup_home.join("toolchains")).expect("rustup home");
        std::fs::create_dir_all(home.join(".config")).expect("config dir");
        std::fs::create_dir_all(home.join(".npm")).expect("npm dir");

        let toolchain = Toolchain {
            cargo: Some(cargo),
            rustup: Some(rustup),
            rustup_home: Some(rustup_home.clone()),
            ..Toolchain::default()
        };
        let binds = toolchain.ro_binds_for(Some(&home), Some(&cargo_home));

        assert!(binds.contains(&cargo_bin));
        assert!(binds.contains(&rustup_home));
        assert!(!binds.contains(&cargo_home));
        assert!(!binds.contains(&home));
        assert!(!binds.iter().any(|bind| home.starts_with(bind)));
        let credentials = cargo_home.join("credentials.toml");
        assert!(!binds.iter().any(|bind| credentials.starts_with(bind)));

        // A host cargo home elsewhere must not make `~/.cargo` a bind root.
        let other_cargo_home = temp.path().join("opt/cargo");
        let binds = toolchain.ro_binds_for(Some(&home), Some(&other_cargo_home));
        assert!(binds.contains(&cargo_bin));
        assert!(!binds.contains(&cargo_home));
        assert!(!binds.iter().any(|bind| credentials.starts_with(bind)));

        let mut unique = binds.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), binds.len(), "binds must be deduplicated");
    }

    #[test]
    fn binds_skip_missing_directories_and_home_config() {
        let temp = tempfile::tempdir().expect("temp dir");
        let home = temp.path().join("home");
        let missing = home.join("missing/bin");
        let config = home.join(".config");
        std::fs::create_dir_all(&config).expect("config dir");

        let toolchain = Toolchain {
            node: Some(missing.join("node")),
            uv: Some(config.join("uv")),
            ..Toolchain::default()
        };
        let binds = toolchain.ro_binds_for(Some(&home), Some(&home.join(".cargo")));

        assert!(!binds.iter().any(|bind| bind.starts_with(&home)));
    }

    #[test]
    fn binds_narrow_generic_home_prefixes_to_the_binary_directory() {
        let temp = tempfile::tempdir().expect("temp dir");
        let home = temp.path().join("home");
        let local_bin = home.join(".local/bin");
        let uv = write_executable(&local_bin, "uv");
        std::fs::create_dir_all(home.join(".local/share")).expect("local share");
        std::fs::write(home.join(".local/share/keyring"), b"secret").expect("secret");
        let npm_prefix = home.join(".npm-global");
        let npm_bin = npm_prefix.join("bin");
        let npm = write_executable(&npm_bin, "npm");
        std::fs::create_dir_all(npm_prefix.join("etc")).expect("npm etc");
        std::fs::write(npm_prefix.join("etc/npmrc"), b"token").expect("npmrc");

        let toolchain = Toolchain {
            uv: Some(uv),
            package_manager: Some(PackageManager {
                kind: PackageManagerKind::Npm,
                binary: npm,
            }),
            ..Toolchain::default()
        };
        let binds = toolchain.ro_binds_for(Some(&home), Some(&home.join(".cargo")));

        assert!(binds.contains(&local_bin));
        assert!(binds.contains(&npm_bin));
        for exposed in [
            home.join(".local"),
            home.join(".local/share"),
            npm_prefix.clone(),
            npm_prefix.join("etc/npmrc"),
        ] {
            assert!(
                !binds.iter().any(|bind| exposed.starts_with(bind)),
                "{} must not be exposed",
                exposed.display()
            );
        }
    }

    #[test]
    fn binds_reject_a_binary_inside_a_generic_home_prefix() {
        let temp = tempfile::tempdir().expect("temp dir");
        let home = temp.path().join("home");
        let uv = write_executable(&home.join(".local"), "uv");

        let toolchain = Toolchain {
            uv: Some(uv),
            ..Toolchain::default()
        };
        let binds = toolchain.ro_binds_for(Some(&home), Some(&home.join(".cargo")));

        assert!(!binds.iter().any(|bind| bind.starts_with(&home)));
        assert!(
            !binds
                .iter()
                .any(|bind| home.join(".local").starts_with(bind))
        );
    }

    #[test]
    fn binds_keep_known_toolchain_roots_under_the_home() {
        let temp = tempfile::tempdir().expect("temp dir");
        let home = temp.path().join("home");
        let node_root = home.join(".nvm/versions/node/v20.11.0");
        let node = write_executable(&node_root.join("bin"), "node");
        std::fs::create_dir_all(node_root.join("lib/node_modules")).expect("node lib");
        let rustup_home = home.join(".rustup");
        std::fs::create_dir_all(rustup_home.join("toolchains")).expect("rustup home");

        let toolchain = Toolchain {
            node: Some(node),
            rustup: Some(PathBuf::from("/usr/bin/rustup")),
            rustup_home: Some(rustup_home.clone()),
            ..Toolchain::default()
        };
        let binds = toolchain.ro_binds_for(Some(&home), Some(&home.join(".cargo")));

        assert!(binds.contains(&node_root));
        assert!(binds.contains(&rustup_home));
        assert!(!binds.contains(&home));
    }

    #[test]
    fn env_points_caches_at_the_scoped_home() {
        let toolchain = Toolchain {
            node: Some(PathBuf::from("/opt/node/bin/node")),
            cargo: Some(PathBuf::from("/usr/bin/cargo")),
            rustup: Some(PathBuf::from("/usr/bin/rustup")),
            rustup_home: Some(PathBuf::from("/home/host/.rustup")),
            ..Toolchain::default()
        };

        let env = toolchain.env(Path::new("/tmp/home")).expect("env");

        assert_eq!(env["CARGO_HOME"], "/tmp/home/.cargo");
        assert_eq!(env["npm_config_cache"], "/tmp/home/.npm");
        assert_eq!(env["UV_CACHE_DIR"], "/tmp/home/.cache/uv");
        assert_eq!(env["RUSTUP_HOME"], "/home/host/.rustup");
        assert!(env["PATH"].starts_with("/opt/node/bin:"));
        for dir in DEFAULT_PATH.split(':') {
            assert!(env["PATH"].split(':').any(|part| part == dir), "{dir}");
        }
        assert!(!env["PATH"].contains("/home/host"));
        assert!(!env["PATH"].contains("/usr/bin:/usr/bin"));
    }

    #[test]
    fn env_rejects_a_relative_scope_home() {
        let toolchain = Toolchain::discover_in(&[], &[]);
        assert!(toolchain.env(Path::new("relative")).is_err());
        assert!(toolchain.env(Path::new("/tmp/../home")).is_err());
        assert!(toolchain.env(Path::new("/tmp/home")).is_ok());
    }

    #[test]
    fn nvm_versions_are_newest_first() {
        let temp = tempfile::tempdir().expect("temp dir");
        let home = temp.path().join("home");
        let versions = home.join(".nvm/versions/node");
        for version in ["v9.1.0", "v18.2.0", "v20.0.0"] {
            std::fs::create_dir_all(versions.join(version).join("bin")).expect("version bin");
        }

        let dirs = nvm_bin_dirs(&home);

        assert_eq!(
            dirs,
            vec![
                versions.join("v20.0.0/bin"),
                versions.join("v18.2.0/bin"),
                versions.join("v9.1.0/bin"),
            ]
        );
    }

    #[test]
    fn discovery_is_harmless_on_this_host() {
        let toolchain = Toolchain::discover();
        let home = host_home();
        for bind in toolchain.ro_binds() {
            assert!(bind.is_absolute(), "{} must be absolute", bind.display());
            if let Some(home) = &home {
                assert!(
                    !home.starts_with(&bind),
                    "{} must not expose the host home",
                    bind.display()
                );
            }
        }
    }
}

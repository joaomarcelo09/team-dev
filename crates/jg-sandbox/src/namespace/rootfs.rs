//! Building the root filesystem of a sandbox and detaching the host root.

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::unistd::pivot_root;

use crate::error::{Result, SandboxError};
use crate::namespace::util::{
    bind_non_recursive, make_mounts_private, mount_proc, mount_tmpfs, remount_read_only,
    verify_read_only, verify_read_only_tree,
};
use crate::namespace::{EGRESS_SOCKET_PATH, HUB_SOCKET_PATH};
use crate::spec::SandboxSpec;

const DEVICE_NODES: [&str; 5] = ["null", "zero", "full", "random", "urandom"];
const USR_SYMLINKS: [&str; 4] = ["bin", "sbin", "lib", "lib64"];
const ETC_PASSWD: &str = "root:x:0:0:root:/lab/home:/bin/sh\nnobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n";
const ETC_GROUP: &str = "root:x:0:\nnogroup:x:65534:\n";

/// Mount the fresh root, populate it, and pivot into it. Runs as sandbox pid 1.
pub(crate) fn setup_rootfs(spec: &SandboxSpec) -> Result<()> {
    let root = crate::state::rootfs_path(&spec.state_dir);

    make_mounts_private()?;
    mount_tmpfs(&root, &format!("mode=0755,size={}", spec.disk.root_bytes))?;

    for dir in ["usr", "etc", "proc", "dev", "tmp", "lab"] {
        create_dir(root.join(dir))?;
    }

    // Mounted before the allowed sockets and binds so that those can shadow
    // paths below them without being hidden by a later mount.
    mount_tmpfs(
        &root.join("tmp"),
        &format!("mode=1777,size={}", spec.disk.tmp_bytes),
    )?;
    mount_proc(&root.join("proc"))?;
    mount_dev(&root.join("dev"))?;

    let mut binds = spec.ro_binds.clone();
    binds.sort_by_key(|path| path.components().count());
    for source in &binds {
        bind_read_only(source, &root)?;
    }

    bind_rw(&spec.lab_dir, &root.join("lab"))?;
    create_usr_symlinks(&root)?;
    write_etc(&root, spec)?;

    let mut sockets = Vec::new();
    for socket in &spec.allowed_sockets {
        sockets.push((socket.as_path(), root.join(strip_root(socket))));
    }
    if let Some(hub_socket) = &spec.hub_socket {
        sockets.push((
            hub_socket.as_path(),
            root.join(strip_root(Path::new(HUB_SOCKET_PATH))),
        ));
    }
    if let Some(egress) = &spec.egress {
        sockets.push((
            egress.proxy_socket.as_path(),
            root.join(strip_root(Path::new(EGRESS_SOCKET_PATH))),
        ));
    }
    for (socket, destination) in sockets {
        bind_socket(socket, &destination)?;
    }

    pivot_into(&root)?;
    create_dir(Path::new("/lab/home"))?;
    Ok(())
}

fn mount_dev(target: &Path) -> Result<()> {
    // No nodev here: the six device nodes below are bind mounts of the host devices.
    mount(
        Some("tmpfs"),
        target,
        Some("tmpfs"),
        MsFlags::MS_NOSUID,
        Some("mode=0755"),
    )
    .map_err(|source| SandboxError::nix("mount /dev", source))?;
    for name in DEVICE_NODES {
        let source = PathBuf::from("/dev").join(name);
        let destination = target.join(name);
        std::fs::File::create(&destination)
            .map_err(|source| SandboxError::io(&destination, source))?;
        nix::mount::mount(
            Some(source.as_path()),
            destination.as_path(),
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )
        .map_err(|error| {
            SandboxError::nix(format!("bind {} into /dev", source.display()), error)
        })?;
    }
    Ok(())
}

fn bind_read_only(source: &Path, root: &Path) -> Result<()> {
    let destination = root.join(strip_root(source));
    create_dir(&destination)?;
    // A non-recursive bind on purpose: a recursive bind also copies the source
    // submounts, and in an unprivileged user namespace those copies are locked
    // and cannot be remounted read-only. A non-recursive bind exposes no
    // submount at all, so nothing writable can hide below a read-only bind.
    bind_non_recursive(source, &destination)?;
    // The mount point path is resolved by the kernel when it appears in
    // mountinfo, so compare against the canonical form.
    let destination = std::fs::canonicalize(&destination)
        .map_err(|source| SandboxError::io(&destination, source))?;
    remount_read_only(&destination)?;
    verify_read_only(&destination)?;
    verify_read_only_tree(&destination)
}

/// A recursive bind, used for the read-write lab so its nested mounts stay visible.
fn bind_recursive(source: &Path, destination: &Path) -> Result<()> {
    nix::mount::mount(
        Some(source),
        destination,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|source| SandboxError::nix(format!("bind mount {}", destination.display()), source))
}

fn bind_rw(source: &Path, destination: &Path) -> Result<()> {
    bind_recursive(source, destination)
}

fn bind_socket(socket: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        create_dir(parent)?;
    }
    std::fs::File::create(destination).map_err(|source| SandboxError::io(destination, source))?;
    nix::mount::mount(
        Some(socket),
        destination,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .map_err(|source| SandboxError::nix(format!("bind socket {}", socket.display()), source))
}

fn create_usr_symlinks(root: &Path) -> Result<()> {
    for name in USR_SYMLINKS {
        let host = Path::new("/").join(name);
        let Ok(target) = std::fs::read_link(&host) else {
            continue;
        };
        let destination = root.join(name);
        if destination.exists() {
            continue;
        }
        symlink(&target, &destination).map_err(|source| SandboxError::io(&destination, source))?;
    }
    Ok(())
}

fn write_etc(root: &Path, spec: &SandboxSpec) -> Result<()> {
    let etc = root.join("etc");
    let files = [
        ("passwd", ETC_PASSWD.to_string()),
        ("group", ETC_GROUP.to_string()),
        ("hostname", format!("{}\n", spec.hostname)),
    ];
    for (name, contents) in files {
        let path = etc.join(name);
        std::fs::write(&path, contents).map_err(|source| SandboxError::io(&path, source))?;
        let mut permissions = std::fs::metadata(&path)
            .map_err(|source| SandboxError::io(&path, source))?
            .permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&path, permissions)
            .map_err(|source| SandboxError::io(&path, source))?;
    }
    Ok(())
}

fn pivot_into(root: &Path) -> Result<()> {
    let old = root.join(".oldroot");
    create_dir(&old)?;
    pivot_root(root, &old).map_err(|source| SandboxError::nix("pivot_root", source))?;
    nix::unistd::chdir("/").map_err(|source| SandboxError::nix("chdir /", source))?;
    umount2("/.oldroot", MntFlags::MNT_DETACH)
        .map_err(|source| SandboxError::nix("detach old root", source))?;
    std::fs::remove_dir("/.oldroot").map_err(|source| SandboxError::io("/.oldroot", source))
}

fn create_dir(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    std::fs::create_dir_all(path).map_err(|source| SandboxError::io(path, source))
}

fn strip_root(path: &Path) -> &Path {
    path.strip_prefix("/").unwrap_or(path)
}

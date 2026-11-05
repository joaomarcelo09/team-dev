# AUR Maintainer Guide

Step-by-step instructions for publishing and updating `jg-team-dev` on the Arch User Repository (AUR).

## 1. AUR Account & SSH Key

1. Register an account at [https://aur.archlinux.org/](https://aur.archlinux.org/).
2. Add your public SSH key (`~/.ssh/id_ed25519.pub`) in Account Settings.
3. Test your SSH connection:
   ```sh
   ssh -T aur@aur.archlinux.org
   ```
4. Ensure packaging tools are installed:
   ```sh
   sudo pacman -S --needed base-devel pacman-contrib devtools namcap
   ```

## 2. Tag a Release

Create and push the version tag to GitHub:

```sh
git tag v0.1.0
git push origin v0.1.0
```

## 3. Update Checksums (`updpkgsums`)

From `packaging/aur/`, update `PKGBUILD` checksums with the tagged tarball:

```sh
cd packaging/aur
updpkgsums
```

## 4. Test Build & Namcap Check

Build the package locally and run namcap to catch packaging errors:

```sh
makepkg -f
namcap PKGBUILD
namcap jg-team-dev-*.pkg.tar.zst
```

## 5. Clean-Chroot Test (`extra-x86_64-build`)

Verify that the package builds cleanly in an isolated environment without missing dependencies:

```sh
extra-x86_64-build
```

## 6. Generate `.SRCINFO`

Regenerate AUR metadata:

```sh
makepkg --printsrcinfo > .SRCINFO
```

## 7. Clone AUR Repository

Clone the AUR package repository:

```sh
git clone ssh://aur@aur.archlinux.org/jg-team-dev.git /tmp/aur-jg-team-dev
```

## 8. Copy Packaging Files

The AUR repository requires `PKGBUILD`, `.install`, and `.SRCINFO`:

```sh
cp PKGBUILD jg-team-dev.install .SRCINFO /tmp/aur-jg-team-dev/
```

## 9. Commit & Push to AUR

Commit the files and push to AUR:

```sh
cd /tmp/aur-jg-team-dev
git add PKGBUILD jg-team-dev.install .SRCINFO
git commit -m "Release v0.1.0"
git push origin master
```

## Updating for New Releases

To publish an update (e.g. `v0.1.1`):

1. Edit `packaging/aur/PKGBUILD`: bump `pkgver=0.1.1` and reset `pkgrel=1`.
2. Tag and push the release on GitHub:
   ```sh
   git tag v0.1.1
   git push origin v0.1.1
   ```
3. Update checksums in `packaging/aur`:
   ```sh
   cd packaging/aur
   updpkgsums
   ```
4. Regenerate `.SRCINFO`:
   ```sh
   makepkg --printsrcinfo > .SRCINFO
   ```
5. Test build (`makepkg -f` or `extra-x86_64-build`).
6. Copy files to the AUR repo clone, commit, and push:
   ```sh
   cd /tmp/aur-jg-team-dev
   git pull
   cp /path/to/team-dev/packaging/aur/{PKGBUILD,jg-team-dev.install,.SRCINFO} .
   git add PKGBUILD jg-team-dev.install .SRCINFO
   git commit -m "Update to v0.1.1"
   git push origin master
   ```

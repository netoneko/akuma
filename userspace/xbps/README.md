# XBPS — Void Linux Package Manager for Akuma

XBPS is the package manager from [Void Linux](https://voidlinux.org/), cross-compiled as 16 statically-linked aarch64-musl binaries. It connects to the official Void Linux repository to install real packages on Akuma.

## Quick Start

```
xbps-install -Sy busybox-static
```

The `SSL_NO_VERIFY_PEER` and `SSL_NO_VERIFY_HOSTNAME` environment variables are set automatically by the kernel for all spawned processes, so HTTPS repository redirects work out of the box.

## Usage Examples

### Sync the repository index

```
xbps-install -S
```

Downloads `aarch64-musl-repodata` (~2 MB) from the Void Linux mirror and imports the repository signing key on first run.

### Install a package

```
xbps-install busybox-static
```

Packages are downloaded, verified (RSA signature), and extracted to `/`.

### Sync and install in one step

```
xbps-install -Sy <package>
```

### Search for packages

```
xbps-query -Rs <pattern>
```

### List installed packages

```
xbps-query -l
```

### Show package details

```
xbps-query -R <package>
```

### Remove a package

```
xbps-remove <package>
```

## What Works

- Repository sync (`-S`) — downloads and decompresses repodata
- RSA signature verification — repository key import and package signature checks
- Package download over HTTP (with automatic HTTPS redirect handling)
- Package extraction — full tar unpacking via libarchive with correct path resolution
- Package database locking and metadata storage in `/var/db/xbps/`

## Configuration

Repository config lives in `/usr/share/xbps.d/00-repository-main.conf`:

```
repository=http://repo-default.voidlinux.org/current/aarch64
```

The architecture is `aarch64-musl` (detected automatically from `uname -m` + the repository index).

## Filesystem Layout

| Path | Purpose |
|------|---------|
| `/usr/share/xbps.d/` | Default repository configuration |
| `/etc/xbps.d/` | User configuration overrides |
| `/var/db/xbps/` | Package database and lock file |
| `/var/db/xbps/keys/` | Imported repository signing keys |
| `/var/cache/xbps/` | Downloaded package archive cache |

## Build

See [docs/BUILD_NOTES.md](docs/BUILD_NOTES.md) for cross-compilation details. The build produces a `dist/xbps.tar` containing all 16 binaries, which is extracted into the disk image during `scripts/populate_disk.sh`.

## Kernel Syscall Support

Getting XBPS to run required implementing or fixing ~25 Linux syscalls in Akuma's kernel, including UDP sockets for DNS, file-backed mmap for proplib, readv for musl's stdio, madvise for musl's allocator, and proper unlinkat/openat error handling for libarchive. Full details in [docs/XBPS_MISSING_SYSCALLS.md](../../docs/XBPS_MISSING_SYSCALLS.md).

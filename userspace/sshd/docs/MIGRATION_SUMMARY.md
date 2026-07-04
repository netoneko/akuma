# SSHD Migration Summary

This document summarizes the migration of the SSH server from kernel space to user space.

## Key Accomplishments

### 1. Ported SSH Server to Userspace
- Created a standalone `sshd` application in `userspace/sshd/`.
- Adapted core SSH-2 protocol logic (`crypto`, `auth`, `keys`, `config`, `protocol`) to run in a `no_std` userspace environment using `libakuma`.
- Implemented a `SshStream` adapter for `libakuma::net::TcpStream` to satisfy `embedded-io-async` requirements.
- Replaced kernel internal RNG with `libakuma::getrandom` syscall.

### 2. Built-in Shell Integration (removed)
- An initial port of the kernel's command execution framework ran directly in
  `sshd` (`ls`, `cat`, `echo`, `ps`, `kill`, `pwd`, `cd`, `uptime`, `curl`,
  `nslookup`, `stats`, `free`, `clear`, `pkg install`, chaining/pipelines/
  redirection) as a fallback for when no external shell was configured.
- **This has since been removed entirely** (see
  [`docs/FLOW.md`](FLOW.md#shell-vs-exec--busybox-is-the-only-shell-now)) —
  `sshd` now always spawns a real shell (busybox `/bin/sh` by default; see
  `config::DEFAULT_SHELL`) for both interactive and one-shot sessions. A
  hand-rolled, untested command interpreter duplicating a real shell wasn't
  worth maintaining once busybox was reliably spawnable.

### 3. Configurable External Shells
- Added support for launching external shell binaries (like `/bin/paws` or busybox's `/bin/sh`).
- Configuration via `/etc/sshd/sshd.conf`: `shell = /bin/paws`.
- CLI override support: `/bin/sshd --shell /bin/sh`.
- There is no fallback shell: if the configured shell fails to spawn, the
  session ends with an error message sent to the client (see `fail_spawn` in
  `protocol.rs`).

### 4. System Enhancements
- **libakuma**: Updated `net::Error` to implement `embedded_io_async::Error`, enabling seamless integration with async I/O crates.
- **Kernel Config**: Added `ENABLE_USERSPACE_SSHD` flag in `src/config.rs` to allow toggling between the legacy kernel server and the new userspace server.
- **Cleanup**: Completely removed the `dropbear` source code and its associated git submodule.

### 5. Build & Deployment
- Integrated `sshd` into the userspace workspace (`userspace/Cargo.toml`).
- Updated `userspace/build.sh` to build and deploy `sshd` to `/bin/sshd` in the bootstrap disk image.

## Usage
The userspace SSH server can be started manually or via the `herd` supervisor:
```bash
/bin/sshd --port 2222 --shell /bin/paws
```
Default port: **2222** (to avoid conflict with kernel SSHD if both are enabled).
Default shell: **`/bin/sh`** (busybox; no built-in fallback — see above).

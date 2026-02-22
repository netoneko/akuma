# SSHD Migration Summary

This document summarizes the migration of the SSH server from kernel space to user space.

## Key Accomplishments

### 1. Ported SSH Server to Userspace
- Created a standalone `sshd` application in `userspace/sshd/`.
- Adapted core SSH-2 protocol logic (`crypto`, `auth`, `keys`, `config`, `protocol`) to run in a `no_std` userspace environment using `libakuma`.
- Implemented a `SshStream` adapter for `libakuma::net::TcpStream` to satisfy `embedded-io-async` requirements.
- Replaced kernel internal RNG with `libakuma::getrandom` syscall.

### 2. Built-in Shell Integration
- Ported the kernel's command execution framework to the userspace `sshd`.
- Migrated and adapted commands: `ls`, `cat`, `echo`, `ps`, `kill`, `pwd`, `cd`, `uptime`, `curl`, `nslookup`, `stats`, `free`, `clear`.
- Added `pkg install` functionality directly into the built-in shell (using logic from `paws`).
- Implemented support for command chaining (`;` and `&&`), pipelines (`|`), and output redirection (`>` and `>>`).

### 3. Configurable External Shells
- Added support for launching external shell binaries (like `/bin/paws`).
- Configuration via `/etc/sshd/sshd.conf`: `shell = /bin/paws`.
- CLI override support: `/bin/sshd --shell /bin/sh`.
- If the configured shell fails to launch, the server automatically falls back to the robust built-in shell.

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
Default shell: **Built-in** (if not specified).

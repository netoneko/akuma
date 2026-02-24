# ubase Implementation Plan for Akuma OS

This document outlines the strategy for integrating the suckless `ubase` (unportable base) utilities into the Akuma userspace environment using a cross-compilation and packaging workflow similar to `tcc` and `musl`.

## 1. Project Structure

The project will be located in `userspace/ubase/` and follow the established "Lib Trick" pattern to prevent Cargo from competing with the C-produced binaries.

```text
userspace/ubase/
├── Cargo.toml
├── build.rs              # Orchestrates download, patching, and compilation
├── src/
│   └── lib.rs            # Empty #![no_std] file (The "Lib Trick")
├── vendor/               # (Git-ignored) Downladed ubase source
├── dist/                 # (Git-ignored) Final ubase.tar
└── docs/
    └── IMPLEMENTATION_PLAN.md
```

## 2. Phase 1: Environment & Tooling

- **Compiler**: Use the host `aarch64-linux-musl-gcc` to ensure compatibility with Akuma's `musl` foundation.
- **Dependencies**: The `build.rs` will depend on the `musl` package being built first to provide headers and `libc.a`.
- **Packaging**: Use the `tar` command with `--format=ustar` and `--no-xattrs` to ensure compatibility with Akuma's `tar` and `pkg install` implementations.

## 3. Phase 2: The `build.rs` Workflow

The `build.rs` will perform the following steps:

1.  **Sourcing**: Download the `ubase` source (typically via `git clone` or fetching a tarball from `git.suckless.org`).
2.  **Configuration**: Patch `config.mk` to point to the correct toolchain:
    - `CC = aarch64-linux-musl-gcc`
    - `LD = aarch64-linux-musl-gcc`
    - `LDFLAGS = -static -Wl,--entry=_start`
    - `CPPFLAGS = -I../../musl/dist/include`
3.  **Compilation**: Execute `make` using the host's GNU Make.
4.  **Staging**: Create a staging directory at `target/.../staging/usr/bin/` and copy the successfully built binaries there.
5.  **Archiving**: Create `dist/ubase.tar` from the staging directory.

## 4. Phase 3: Incremental Porting

Since `ubase` is Linux-specific, tools will be enabled in tiers based on kernel support:

### Tier 1: Minimal Dependencies (Immediate)
- `dd`, `respawn`, `watch`, `clear`, `pagesize`.
- These rely on standard POSIX file I/O and should work with existing syscalls.

### Tier 2: Process/System Info (Requires `procfs` extension)
- `ps`, `uptime`, `pidof`, `killall5`.
- **Requirement**: Extend `src/vfs/proc.rs` to provide `/proc/uptime`, `/proc/loadavg`, and more detailed `/proc/<pid>/` entries (stat, cmdline).

### Tier 3: Privileged Operations (Requires new Syscalls)
- `mount`, `umount`, `pivot_root`, `reboot`, `passwd`.
- **Requirement**: Implement `sys_mount`, `sys_umount`, and `sys_reboot` in `src/syscall.rs`.

## 5. Phase 4: Integration & Testing

1.  **Disk Integration**: Add `ubase` to the `userspace/build.sh` script.
2.  **Package Delivery**: Stage `ubase.tar` in `bootstrap/` to allow installation via `pkg install ubase`.
3.  **Smoke Test**: Verify basic functionality of `dd` and `watch` in the Akuma shell (`paws`).

## 6. Success Criteria

- `ubase.tar` is successfully generated during the `cargo build` of the userspace workspace.
- Tier 1 utilities are executable and functional within the Akuma environment.
- The build system is reproducible and correctly handles the cross-compilation toolchain.

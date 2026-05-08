# Syscall errno compliance — change log

This document summarizes the kernel changes made so failing syscalls return **Linux AArch64–compatible** values in **`x0`**: on failure, **`x0 = -(errno)`** as an unsigned bit pattern (same convention as Linux). Previously, many paths returned **`!0u64`** (`(u64)-1`), which userspace typically decodes as **`errno == EPERM (1)`**, masking the real failure reason.

## Motivation

- **`!0u64`** is not “generic error”; it is **`-1`** as a signed long, i.e. **`EPERM`** when decoded as negated errno.
- Programs (musl, Go runtime, etc.) branch on **`errno`**; wrong codes cause subtle bugs (e.g. treating failures as permission errors or dereferencing bogus “success” values).

## Shared infrastructure

| Item | Location |
|------|----------|
| **`neg_errno(i32) -> u64`** | [`src/syscall/mod.rs`](../src/syscall/mod.rs) — central helper for encoding **`-(positive libc errno)`** |
| **Additional `u64` errno constants** | Same file: **`EACCES`**, **`ENOEXEC`**, **`EADDRINUSE`** (reserved; may be unused until bind/listen distinguish duplicate bind), plus reordering/comments for ABI clarity |
| **`libc_errno` extensions** | [`crates/akuma-net/src/socket.rs`](../crates/akuma-net/src/socket.rs) — **`EPERM`**, **`ESRCH`**, **`ENOEXEC`**, **`EACCES`**, **`EFAULT`**, **`EEXIST`**, **`EMFILE`**, **`EADDRINUSE`**, etc. |
| **Host tests for errno numbers** | [`crates/akuma-net/src/tests.rs`](../crates/akuma-net/src/tests.rs) — `errno_values_match_linux` |

## File-by-file summary

### [`src/syscall/net.rs`](../src/syscall/net.rs)

- **`sys_socket`**: socket table full → **`EMFILE`**; orphan path after alloc → **`ESRCH`**.
- **`sys_bind` / `sys_listen`**: propagate **`socket_bind` / `socket_listen`** **`Result<(), i32>`** via **`neg_errno`**; **`len < 16`** → **`EINVAL`**; invalid fd → **`EBADF`**.
- **`sys_connect`**: same pattern; short length → **`EINVAL`**.
- **`sys_accept` / `sys_accept4`**: **`EBADF`** / **`ESRCH`** instead of **`!0`**; socket errors via **`neg_errno`**.
- **`sys_getsockname` / `sys_getpeername` / `sys_sendmsg` / `sys_recvmsg` / `sys_sendto` / `sys_recvfrom`**: use shared constants and **`neg_errno`** where applicable; UDP without peer uses **`EDESTADDRREQ`** where appropriate.
- **`sys_resolve_host`** (custom syscall **`nr::RESOLVE_HOST`**, 300): DNS failure → **`ENOENT`** (documented in code as Akuma-specific contract; not a Linux syscall).

### [`src/syscall/mem.rs`](../src/syscall/mem.rs)

- **`mmap`**: **`len == 0`** → **`EINVAL`**; no owning process → **`ESRCH`**; unaligned **`MAP_FIXED`** address → **`EINVAL`**; **`alloc_mmap`** / batch page alloc failure → **`ENOMEM`** (replacing **`!0`**).
- **`munmap`**: no process → **`ESRCH`**.

### [`src/syscall/proc.rs`](../src/syscall/proc.rs)

- **`sys_setpgid`**: unknown pid → **`ESRCH`** (was **`ENOENT`**); missing current pid → **`ESRCH`**.
- **`sys_getpgid`**: removed TID / bogus-pgid fallbacks; missing process → **`ESRCH`**.
- **`sys_setsid`**: no current process → **`ESRCH`** (with comment vs Linux **`setsid`** semantics).
- **`sys_getpid`**: defensive no-pid → **`ESRCH`** (Linux **`getpid`** cannot fail; this is a kernel sentinel).
- **`sys_getppid`**: no process → **`ESRCH`**.
- **`sys_getrandom`**: RNG failure → **`EIO`**.
- **`clone` (fork path)**: no parent process → **`ESRCH`**; fork failure → **`ENOMEM`**.
- **`do_execve`**: **`read_file` / `file_size`** errors → **`fs_error_to_errno`**; **`replace_image`** string errors → **`ENOEXEC`** if message indicates ELF load failure, else **`ENOMEM`**; missing **`current_process`** → **`ESRCH`**.
- **`sys_spawn` / `sys_spawn_ext`**: failure → **`ENOMEM`** or **`EINVAL`** for null options; **`sys_kill`**: **`pid <= 1`** → **`EPERM`** (unsupported group semantics).

### [`src/syscall/fs.rs`](../src/syscall/fs.rs)

- **`fs_error_to_errno`**: **`FsError::PermissionDenied`** → **`EACCES`** (Linux file-access convention), not **`EPERM`**.
- **`/dev/urandom`** read failure → **`EIO`**.
- Generic unsupported fd in **`read`/`write`** → **`EBADF`** where applicable.
- **`sys_lseek`**: distinguish bad fd vs invalid **whence**/position (**`EBADF`** vs **`EINVAL`**).
- **`sys_fstat`**: metadata failure → **`ENOENT`**; unknown fd type → **`EBADF`**.
- **`sys_newfstatat` / `sys_faccessat2`**: **`ESRCH` / `EBADF`** for cwd/dirfd issues instead of **`!0`**.
- **`sys_openat`**: special device paths without process → **`ESRCH`**; normal open without process → **`ESRCH`**.
- **`sys_close`**: unknown fd → **`EBADF`**; no process → **`ESRCH`**.
- **`sys_renameat` / `sys_renameat2` / `sys_linkat`**: use **`fs_error_to_errno`** on **`FsError`**.
- **`sys_getdents64`**: list dir errors → **`fs_error_to_errno`**; bad fd / no process → **`EBADF` / `ESRCH`**.
- **`sys_fchdir` / `sys_chdir`**: no process → **`ESRCH`**.

### [`src/syscall/container.rs`](../src/syscall/container.rs)

- **`sys_kill_box`**: failure → **`ESRCH`** (unknown box).
- **`sys_reattach`**: failure → **`ESRCH`** (unknown pid).

### [`src/syscall/term.rs`](../src/syscall/term.rs)

- **`sys_ioctl`**: no current process → **`ESRCH`**.

## Custom syscall contract

| Syscall number | Name | Failure convention |
|----------------|------|---------------------|
| **300** (`nr::RESOLVE_HOST`) | Resolve hostname to IPv4 | DNS / resolution failure → **`ENOENT`** (see comment in [`net.rs`](../src/syscall/net.rs)) |

## In-kernel regression test

- **`test_syscall_errno_compliance`** in [`src/process_tests.rs`](../src/process_tests.rs): exercises **`mmap(0,0,…)` → EINVAL**, **`bind`** with bad fd → **`EBADF`** or **`EFAULT`**, **`setpgid`** on nonexistent pid → **`ESRCH`**, and asserts results are not **`EPERM`** from the old **`!0`** sentinel.
- Invoked from **`run_all_tests()`** in the same file.

## What was not changed

- **`!0u64`** remains where it denotes **non-error semantics** (e.g. **`RLIM_INFINITY`** in prlimit), or **sentinel atomic values** for syscall tracing (**`current_syscall`**), not syscall **return values**.

## Verification commands

```bash
cargo check --release
cargo build --release
cargo test --target "$(rustc -vV | grep '^host:' | cut -d' ' -f2)" -p akuma-net
```

---

*Generated to document the syscall errno compliance pass; align future changes with this ABI.*

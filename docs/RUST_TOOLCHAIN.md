# Rust Toolchain (`rustc`) on Akuma — Missing Syscalls & Fixes

Syscalls and kernel changes needed to run the Rust compiler (`rustc`, the
`aarch64-alpine-linux-musl` toolchain) on Akuma, plus the known issues that
still block a full compile-and-link.

## Status

| Stage | State |
|-------|-------|
| `rustc` loads + runs codegen (`.rcgu.o` produced) | ✅ works (needs ≥2 GB RAM) |
| Linker spawn handshake (`socketpair`) | ✅ **fixed** (this doc, §1) |
| `lseek`/`readlinkat` EINVAL during link probe | ✅ benign or **fixed** (§4) |
| Linker spawn handshake (libstd `fork` + O_CLOEXEC error pipe) | ❌ parent panics `the CLOEXEC pipe failed: … Bad file descriptor` (EBADF) — under investigation (§4) |
| Final link (invoking `cc`/`ld`) | ❌ userspace `SIGSEGV` in `rustc` (older trace; now masked by the EBADF above) |

---

## 1. `socketpair` (AArch64 syscall nr 199) — FIXED

### Symptom

```
$ rustc -C linker=clang hello.rs -o /tmp/hello_rust
error: could not exec the linker `clang`
  = note: Function not implemented (os error 38)
```

`os error 38` is `ENOSYS`. The message is **misleading** — the failure happens
*before* `clang` is ever exec'd. The kernel log showed the real cause:

```
[ENOSYS] nr=199 pid=74 args=[0x1, 0x80005, 0x0, 0xf0ce3010, ...]
```

### Diagnosis

`nr=199` is `socketpair`. AArch64 uses the `asm-generic` syscall table
(`include/uapi/asm-generic/unistd.h`), cross-checked locally against
`aarch64-linux-musl/include/asm-generic/unistd.h` and zig's resolved
`aarch64-linux-any/asm/unistd_64.h`:

```
__NR_socket      198   __SYSCALL(sys_socket)
__NR_socketpair  199   __SYSCALL(sys_socketpair)   ← was MISSING in Akuma
__NR_bind        200   __SYSCALL(sys_bind)
__NR_listen      201   __SYSCALL(sys_listen)
```

Akuma's `src/syscall/mod.rs` defined `SOCKET=198`, `BIND=200`, … matching the
spec everywhere except there was no arm for `199`, so it fell through to the
default `ENOSYS` handler.

Decoding the logged args `[0x1, 0x80005, 0x0, 0xf0ce3010]` against the kernel
prototype `sys_socketpair(int domain, int type, int protocol, int *usockvec)`:

- `domain = 1` → `AF_UNIX`
- `type = 0x80005` → `SOCK_SEQPACKET (5) | SOCK_CLOEXEC (0x80000)`
- `protocol = 0`
- `usockvec = 0xf0ce3010` (output fd-pair pointer)

This is **Rust std's child-spawn setup**: before forking to exec the linker,
libstd creates an `AF_UNIX`/`SOCK_SEQPACKET` socketpair as the IPC channel used
to hand the child's exec-errno back to the parent. The `ENOSYS` aborted rustc
before it reached the exec — hence the misleading "could not exec the linker".

### Design

Akuma has **no general AF_UNIX socket support** — `akuma-net` only wraps smoltcp
TCP/UDP, and `sys_socket` rejects any `domain != AF_INET` with `EAFNOSUPPORT`.
Building a full AF_UNIX stack would be heavyweight and pull smoltcp where it
doesn't belong.

Instead, **each socketpair is backed by two existing kernel pipes**
(`src/syscall/pipe.rs`), which already provide buffering, fork ref-counting,
EOF, SIGPIPE, and pollers/wakers. A socketpair is bidirectional, so we use two
unidirectional pipes and give each endpoint one read pipe + one write pipe:

```
pipe X carries endpoint0 -> endpoint1
pipe Y carries endpoint1 -> endpoint0

Endpoint 0 = { rx: X, tx: Y }
Endpoint 1 = { rx: Y, tx: X }
```

`pipe_create()` starts each pipe at `write_count=1, read_count=1`, which is
exactly one writer + one reader per direction — no manual ref adjustment.

A new `FileDescriptor` variant carries the two pipe IDs:

```rust
// crates/akuma-exec/src/process/types.rs
UnixSocket { rx: u32, tx: u32 },   // rx/tx are pipe IDs
```

> **SEQPACKET caveat:** byte-stream pipes do not preserve message boundaries the
> way real `SOCK_SEQPACKET` does. This is sufficient for libstd's single
> fixed-size errno handshake (and EOF-on-success) and unblocks rustc, but it is
> an approximation, not a fully conformant SEQPACKET.

### Implementation

| File | Change |
|------|--------|
| `crates/akuma-exec/src/process/types.rs` | New `UnixSocket { rx, tx }` variant on `FileDescriptor` |
| `crates/akuma-exec/src/process/fd.rs` | Fork-clone (`clone_deep_for_fork`) bumps both pipe refs; `close_all` closes both directions — via the runtime vtable (`pipe_clone_ref`, `pipe_close_read`, `pipe_close_write`) |
| `src/syscall/mod.rs` | `pub const SOCKETPAIR: u64 = 199;` + dispatch arm (note `usockvec` is the **4th** arg: `args[3]`) |
| `src/syscall/net.rs` | `sys_socketpair` handler — AF_UNIX only; `SOCK_STREAM`/`SOCK_SEQPACKET`; honors `SOCK_CLOEXEC`/`SOCK_NONBLOCK`; creates two pipes; installs two `UnixSocket` fds; copies the pair to userspace; rolls back on `EFAULT` |
| `src/syscall/fs.rs` | `read` from `rx`, `write` to `tx`; `close`/`close_range`/`dup`/`dup3` routing |
| `src/syscall/proc.rs` | exec-time close-on-exec routing |
| `src/syscall/poll.rs` | `EPOLLIN` ← `pipe_can_read(rx)`, `EPOLLOUT` ← `pipe_can_write(tx)` |
| `src/vfs/proc.rs` | `/proc/<pid>/fd` display string (exhaustive match — required to compile) |
| `src/process_tests.rs` | Four boot-suite self-tests |

The handler (`src/syscall/net.rs`):

```rust
pub(super) fn sys_socketpair(domain: i32, sock_type: i32, _proto: i32, sv_ptr: u64) -> u64 {
    let base_type = sock_type & 0xFF;
    let cloexec = sock_type & 0x80000 != 0;
    let nonblock = sock_type & 0x800 != 0;
    // Only AF_UNIX (1); accept SOCK_STREAM (1) and SOCK_SEQPACKET (5).
    if domain != 1 || (base_type != 1 && base_type != 5) { return EAFNOSUPPORT; }
    if !validate_user_ptr(sv_ptr, 8) { return EFAULT; }
    let proc = match akuma_exec::process::current_process() { Some(p) => p, None => return ESRCH };

    let px = super::pipe::pipe_create();
    let py = super::pipe::pipe_create();
    let fd0 = proc.alloc_fd(FileDescriptor::UnixSocket { rx: px, tx: py });
    let fd1 = proc.alloc_fd(FileDescriptor::UnixSocket { rx: py, tx: px });
    if cloexec { proc.set_cloexec(fd0); proc.set_cloexec(fd1); }
    if nonblock { proc.set_nonblock(fd0); proc.set_nonblock(fd1); }

    let fds = [fd0 as i32, fd1 as i32];
    if copy_to_user_safe(sv_ptr, &fds, 8).is_err() { /* roll back fds + pipes */ return EFAULT; }
    0
}
```

### Kernel self-tests

Registered in `run_all_tests()` in `src/process_tests.rs`:

- `test_socketpair_not_enosys` — `handle_syscall(199, …)` returns ≠ `ENOSYS`
  (the direct regression guard for the rustc failure; a null `sv` yields
  `EFAULT`, which still proves the arm is wired).
- `test_socketpair_domain_rejected` — `AF_INET` → `EAFNOSUPPORT`.
- `test_socketpair_bidirectional` — data written to each endpoint's `tx` is
  readable on the peer's `rx`, both directions independent.
- `test_socketpair_close_refcount` — closing both endpoints drives both backing
  pipes to `DESTROY`; redundant closes don't panic.

---

## 2. Verification (2026-05-30)

Verified on a fresh `INSTANCE=1 MEMORY=2048` boot against a disk copy
(`uptime` inside the VM confirmed `up 0:00:20` — genuinely the new kernel, not a
stale instance):

- All four `socketpair_*` self-tests **PASSED**.
- End-to-end `rustc /tmp/hello.rs -o /tmp/hello_out`: the kernel log shows

  ```
  [syscall] socketpair(AF_UNIX) = (11, 12)
  [FORK-DBG] fork_process ENTRY
  [pipe] clone_ref id=40 ...
  [pipe] clone_ref id=41 ...
  ```

  rustc's spawn handshake now **succeeds** (previously `[ENOSYS] nr=199`), and
  the socketpair's pipes clone correctly across the `fork` that spawns the
  linker.

### Reproduction notes

- `rustc` needs **≥2 GB RAM** (`MEMORY=2048`). Under the default 256 MB it fails
  to load its shared libs (`libLLVM.so.21.1`, `librustc_driver`) with
  "Out of memory".
- The in-kernel SSH server is a **builtin-command shell**, not a POSIX shell. It
  execs PATH binaries for unknown commands (so `rustc …` runs) but does **not**
  support `2>&1`, `$?`, or complex quoting. Use `write <file> <text>` / `echo …
  > file` to stage source files.
- For parallel/non-disruptive testing, boot a second instance with shifted ports
  and a **disk copy** so you don't collide with a running VM's exclusive
  `disk.img` lock:

  ```bash
  cp disk.img /tmp/disk_test.img
  MEMORY=2048 DISK=/tmp/disk_test.img INSTANCE=1 cargo run --release
  # ssh on port 2322 (= 2222 + 100*INSTANCE)
  ```

---

## 3. Known remaining blockers (out of scope for the socketpair fix)

### 3a. Userspace `SIGSEGV` during link (older trace — now masked by §4)

An earlier trace, after `socketpair` succeeded and rustc forked to spawn the
linker, showed `rustc` (pid 77) hitting a userspace data abort:

```
[T93.93] [WILD-DA] pid=77 FAR=0xf0f1a298 ELR=0x30048a90 last_sc=...
[T93.94] [Fault] Data abort from EL0 at FAR=0xf0f1a298, ELR=0x30048a90, ISS=0x46
[Fault] Process 77 (rustc) SIGSEGV after 0.02s
```

This is an **EL0 (userspace) fault** — the faulting PC (`~0x30048a90`) is in the
musl/libc region and the accessed address (`0xf0f1a298`) maps to no region
("WILD-DA"). It is *not* a kernel crash (the VM stayed up). **As of the
2026-05-30 runs (§4) this is no longer the symptom reached** — the spawn now
fails earlier, in the libstd `fork`+exec handshake (EBADF). Keep this note for
history; revisit only once §4 is resolved.

### 3b. `clang` — installed and works for C

The earlier claim that `clang` was not installed is **outdated**. `clang` is
present and compiles C fine inside the VM (verified: `clang hello.c` produces a
working binary). So `rustc -C linker=clang` does reach the linker — the current
blocker is *not* a missing `clang`, it is the libstd spawn handshake (§4).

---

## 4. EINVAL audit + libstd-spawn `EBADF` (2026-05-30)

Triggered by `rustc -v -C linker=clang hello.rs`, which panics:

```
thread 'rustc' panicked at library/std/src/sys/process/unix/unix.rs:154:
the CLOEXEC pipe failed: Os { code: 9, kind: Uncategorized, message: "Bad file descriptor" }
  … rustc_codegen_ssa::back::link::link_binary …
error: the compiler unexpectedly panicked. this is a bug.
```

### 4a. EINVAL audit (`rustc1.log`) — not the cause

All `EINVAL`s in the log fall into three buckets; none break rustc:

1. **Boot self-tests** (`pid=0 tid=0 ELR=?`) — the kernel deliberately drives
   `mmap`/`futex`/`io_setup` to their EINVAL paths. Expected.
2. **`readlinkat` (nr 78), ~337×, runtime** — `args=[AT_FDCWD, path, buf, bufsz]`
   with `bufsz` cycling `0xff8→0x1000` (a path-canonicalization buffer loop).
   `sys_readlinkat` returns `EINVAL` when the path **exists but is not a
   symlink** (`src/syscall/fs.rs`), which is exactly POSIX `readlink(2)`
   behavior. rustc tolerates it and continues (`PSTATS` shows `readlinkat`
   taking <100 ms total). **Benign — no change.**
3. **`lseek` (nr 62), 1×** — `lseek(fd=2 /*stderr*/, 0, SEEK_CUR)`, the standard
   "is stderr seekable?" probe. The handler returned `EINVAL` for every
   non-`File` fd. POSIX requires **`ESPIPE`** on a pipe/tty/socket. **Fixed:**
   `sys_lseek` now returns `ESPIPE` for non-seekable fd types and keeps `EINVAL`
   only for a real file with an invalid offset/whence. Guard:
   `test_lseek_nonseekable_returns_espipe`.

### 4b. The real blocker — `EBADF` reading the libstd CLOEXEC error pipe

`code: 9` is `EBADF`, **not** `EINVAL`. This is libstd's `fork`+exec fallback
(not `posix_spawn`): the parent reads the read-end of an `O_CLOEXEC` pipe that
the child either closes on a successful `exec` (→ parent reads EOF = success) or
writes the exec errno into. The parent's `read()` returning `EBADF` means **the
read-end fd is absent from the parent's fd table** at read time.

What the investigation established:

- **Pipe refcounting is correct.** `test_pipe_clone_ref_then_double_close`
  models the exact spawn lifecycle (fork clone_ref → child dup3/close → child
  exec closes its CLOEXEC end → parent close) and passes. A *destroyed* pipe
  would surface as `read()→0` (EOF), **not** `EBADF`.
- **`clang` itself works** (`clang hello.c` succeeds), so the child's `execve`
  succeeds — the parent *should* read EOF, yet gets `EBADF`. That isolates the
  fault to libstd's `fork`+CLOEXEC-pipe handshake (the in-kernel shell's exec
  path, which does **not** use this handshake, runs clang fine).
- **`fork` deep-clones the fd table** (`fork_process` →
  `clone_deep_for_fork`), so the child cannot remove the parent's entry. The
  read-path handles both `PipeRead` and `UnixSocket`, so this is not a
  missing-fd-type fallthrough.

Conclusion: the parent's CLOEXEC pipe read-end is being removed from its fd
table by something at the process/threading level (rustc is multithreaded; the
spawn is from a worker thread sharing the fd-table `Arc`). The exact remover is
not determinable from static analysis alone, so a **bounded, always-on
diagnostic** was added: `read()` returning `EBADF` now logs the first 32
occurrences as

```
[read-ebadf] <reason> pid=… tid=… fd=… buf=…
```

where `<reason>` is `no-current-process` or `fd-not-in-table`. The next run will
name the exact pid/tid/fd and which of the two paths fires — that pins the bug.

### 4c. Fix landed alongside — CLOEXEC closed only on successful `exec`

`do_execve` previously called `close_cloexec_fds()` **before** `replace_image`.
A failed image load (bad ELF → `ENOEXEC`, OOM → `ENOMEM`) then returned to a
process whose close-on-exec fds had already been torn down. POSIX closes
`O_CLOEXEC` fds only at the **point of no return** (a *committed* image
replacement); a failed `execve` must leave the fd table intact — for a libstd
child that is what preserves its error-report pipe so it can hand the errno
back. **Fixed:** the CLOEXEC sweep now runs only after `replace_image` succeeds.
Guard: `test_failed_exec_preserves_cloexec_fds` (stages a non-ELF file, attempts
`do_execve`, asserts the cloexec fd survives the failure).

This is correct hygiene and matters for failed execs, but note it does **not**
by itself explain the `clang` `EBADF` (there the child's exec *succeeds*) — that
remains pending the §4b diagnostic.

---

## References

- Syscall dispatch + errno consts (`ESPIPE`): `src/syscall/mod.rs`
- socketpair handler: `src/syscall/net.rs`
- Pipe backing: `src/syscall/pipe.rs`
- `read()` EBADF diagnostic, `sys_lseek` (ESPIPE), `sys_readlinkat`: `src/syscall/fs.rs`
- `do_execve` CLOEXEC-on-success ordering: `src/syscall/proc.rs`
- FD table / fork: `crates/akuma-exec/src/process/{types,fd}.rs`, `fork_process`/`replace_image` in `crates/akuma-exec/src/process/{mod,image}.rs`
- Self-tests: `src/process_tests.rs` (`test_lseek_nonseekable_returns_espipe`, `test_failed_exec_preserves_cloexec_fds`, `test_pipe_clone_ref_then_double_close`)
- Related: `docs/APK_MISSING_SYSCALLS.md`, `docs/FORK_MMAP_AND_WAIT_STATUS_FIX.md`

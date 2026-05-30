# Rust Toolchain (`rustc`) on Akuma — Missing Syscalls & Fixes

The Rust compiler (`rustc`, the `aarch64-alpine-linux-musl` toolchain) **runs
end-to-end on Akuma**: as of 2026-05-31,
`rustc -C linker=clang hello.rs -o /tmp/hello_rust` compiles, links via
`clang-21`→`ld`, and the resulting binary executes (`/tmp/hello_rust` →
`Hello from Akuma!`).

This doc records the syscalls and kernel changes that got it there. The fixes
landed in dependency order — each unblocked the next failure: `socketpair` (§1),
the `lseek`/`readlinkat` EINVAL audit and `lseek`→`ESPIPE` fix (§4a), CLOEXEC
close-on-success ordering (§4c), the multithreaded-`fork` address-space
replication (§4b′), and `UnixSocket` routing in the socket recv/send syscalls
(§4d). The only open item is **performance** (slow fork CoW — §5b); correctness
is solid.

## Status

| Stage | State |
|-------|-------|
| `rustc` loads + runs codegen (`.rcgu.o` produced) | ✅ works (needs ≥2 GB RAM) |
| Linker spawn handshake (`socketpair`) | ✅ **fixed** (this doc, §1) |
| `lseek`/`readlinkat` EINVAL during link probe | ✅ benign or **fixed** (§4) |
| Linker spawn fork (multithreaded rustc) | ✅ **fixed** (§4b′) — fork now replicates the whole thread group's address space (lazy regions by `tgid` + every sibling thread's eager `mmap_regions`); child no longer SIGSEGVs and exec's `clang-21` → `ld` |
| CLOEXEC handshake read (`recvmsg` on the socketpair) | ✅ **fixed** (§4d) — socket recv/send syscalls now route `UnixSocket` fds to the backing pipes; was the real `the CLOEXEC pipe failed: … Bad file descriptor` |
| Final link (invoking `clang`→`ld`) | ✅ **works** — completes and produces a runnable binary |
| **End-to-end `rustc hello.rs`** | ✅ **WORKS (verified 2026-05-31)** — `rustc -C linker=clang hello.rs -o /tmp/hello_rust` compiles, links via `clang-21`→`ld`, and the binary runs: `/tmp/hello_rust` → `Hello from Akuma!` |

> **Perf caveat:** an end-to-end compile takes ~3 min, dominated by the fork
> CoW copying the whole ~75k-page address space on each `fork` (~30 s `mmap` +
> ~32 s `munmap`). Correctness is solid; speed is the open item — see §5b.

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

## 3. Blockers found while bringing up the toolchain (all resolved)

### 3a. Userspace `SIGSEGV` during link — RESOLVED (was the multithreaded-fork bug)

An early trace, after `socketpair` succeeded and rustc forked to spawn the
linker, showed `rustc` (pid 77) hitting a userspace data abort:

```
[T93.93] [WILD-DA] pid=77 FAR=0xf0f1a298 ELR=0x30048a90 last_sc=...
[T93.94] [Fault] Data abort from EL0 at FAR=0xf0f1a298, ELR=0x30048a90, ISS=0x46
[Fault] Process 77 (rustc) SIGSEGV after 0.02s
```

This `WILD-DA` (faulting PC `~0x30048a90` in `ld-musl`, accessed address mapping
to no region) was later **caught live under gdb and identified as the
multithreaded-`fork` bug** — the forked child walking musl's thread list into a
sibling thread's stack that `fork` hadn't replicated. **Fixed in §4b′.** Kept
here because this is the trace that first pointed at it.

### 3b. `clang` — installed and works for C

The earlier claim that `clang` was not installed is **outdated**. `clang` is
present and compiles C fine inside the VM (verified: `clang hello.c` →
`Hello, Akuma!`), and `rustc -C linker=clang` now drives it through to a working
binary. The blocker was never a missing `clang` — it was the libstd spawn path
(§4b′ + §4d), now fixed.

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

The `EBADF` is a **downstream symptom**: the spawned child crashes before it can
`exec`, so the handshake never completes. The kernel log of the failing run
(`rustc2.log`) shows it directly — right after `fork_process` (parent pid 72)
the child **pid 77 takes a fatal data abort**:

```
[FORK-DBG] fork_process EXIT ok
[DA-MISS] pid=77 ppid=72 va=0xf0f1a298 ... parent_has_va=false
[WILD-DA] pid=77 FAR=0xf0f1a298 ELR=0x30048a90 last_sc=...
  # child's last syscalls: 96 set_tid_address → 77, 135 rt_sigprocmask → 0
[Fault] Process 77 (rustc) SIGSEGV after 0.01s
```

(A bounded `[read-ebadf]` diagnostic was added to `read()` to confirm the parent
side; it does *not* fire on the two `fd-absent` paths, consistent with the EBADF
being a consequence of the dead child rather than a lost parent fd.)

### 4b′. Root cause (caught live under gdb): multithreaded-fork drops sibling thread stacks

Reproduced the crash under QEMU's gdbstub (`INSTANCE=1 GDB=1`, lldb over the
gdb-remote protocol), breaking at the fatal-fault handler. Disassembling the
faulting child code at `ELR=0x30048a90` (in `ld-musl`) shows a **circular
thread-list walk**:

```
mrs  x24, TPIDR_EL0 ; sub x24, x24, #0xc8   ; x24 = list anchor (this pthread)
ldr  x20, [x24, #0x10]                       ; x20 = first node (->next)
loop:
  str  w0,  [x20, #0x20]                      ; <-- FAULTS: node->field@0x20 = w0  (FAR = x20+0x20)
  ldr  x20, [x20, #0x10]                      ; x20 = node->next
  cmp  x20, x24 ; b.ne loop                   ; until back to anchor
```

Combined with the child's last two syscalls (`set_tid_address`, `rt_sigprocmask`)
this is **musl's `fork()` child-side thread-list fixup**: the forked child walks
the thread list it inherited from the parent — which still links the parent's
*other* rustc worker threads' `pthread` structs — and faults on a sibling node
(`0xf0f1a278`) whose page is **not present in the child**.

Why it's absent — the kernel-side bug (`crates/akuma-exec/src/process/mod.rs`):

- Each `clone_thread` worker gets its **own `Process` struct with its own `pid`**
  but a **shared address space** and shared `tgid`. Its `mmap_regions` is a
  private `Vec`, and the *leader* thread (`pid == tgid`) is the one that runs
  `pthread_create` and therefore `mmap`s the worker stacks.
- `mmap` splits its bookkeeping by tracking path:
  - **lazy** anonymous regions (>256 pages, `MAP_NORESERVE`, …) go to
    `LAZY_REGION_TABLE` keyed by **`proc.tgid`** (shared across the group);
  - **eager** regions — including the *small* anonymous mappings musl uses for
    pthread stacks/TLS (e.g. `len=0x6000`, `0x100000`) — are pushed onto the
    **calling thread's** private `proc.mmap_regions`.
- `fork_process` enumerated lazy regions under **`parent.pid`** and CoW-shared
  only the **forking thread's own** `mmap_regions`. When the forking thread is a
  *worker* (`pid 72`, `tgid 70`):
  - the lazy lookup under `pid 72` missed the group's `tgid 70` regions, and
  - the eager pthread stacks (mmap'd by the leader, **pid 70**) were on pid 70's
    struct, never copied.

  Confirmed from the log: the faulting `va=0xee402058` lies in
  `[mmap] pid=70 … = 0xee402000 (eager)` — a leader-thread eager anon mapping,
  invisible to a fork from pid 72. (`parent_has_va=false` only reports that the
  *forking thread's lazy table* lacks it; the page is live in the shared page
  tables.)

On Linux, `fork()` duplicates the entire address space, so all sibling stacks
(inert but mapped) survive and the walk succeeds. Akuma's region-enumerated CoW
was lossy for a multithreaded parent on **two** axes.

**Fix (two parts, both in `fork_process`, CoW + eager-copy paths):**

1. Enumerate lazy regions by **`parent.tgid`** (not `parent.pid`) — captures the
   whole group's lazy mappings.
2. CoW-share (and RO-demote) the **eager `mmap_regions` of every sibling thread**
   in the group: iterate `table::for_each_process`, match `p.tgid == parent.tgid`,
   union their `mmap_regions`. (`for_each_process` runs IRQs-disabled and forbids
   allocation in its callback — and Akuma is single-CPU — so ranges are collected
   into a pre-reserved `Vec` with no in-callback allocation, then shared after.)

Together these replicate every sibling thread's stack/TLS into the child, so
musl's thread-list fixup dereferences valid (CoW) memory. For a single-threaded
process `pid == tgid` and there are no siblings, so both parts are **no-ops** on
the common path (no regression risk).

> **Remaining (theoretical) gap:** this still enumerates *tracked regions* rather
> than walking the page tables, so any present page not covered by a lazy region
> or some thread's `mmap_regions` (e.g. internal TLS/`process_info` pages) would
> still be missed. None are implicated in the rustc fault. The fully-robust
> long-term fix is a page-table walk that CoW-shares every present user page;
> left as follow-up since it is a larger, higher-blast-radius change to a
> critical path.

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

This is correct hygiene for failed execs, but it is **not** the `clang` blocker.

### 4d. The CLOEXEC-pipe `EBADF`, finally — `recvmsg` on the socketpair

With the multithreaded-fork fix (§4b′) the spawned child no longer SIGSEGVs: it
exec's `clang-21`, which itself forks and exec's `ld` — visible in `rustc5.log`
(`[FORK-COW] shared 75049 pages`, then `execve(".../clang-21")`,
`execve(".../bin/ld")`, no `WILD-DA`/SIGSEGV). But rustc still panicked with the
same `the CLOEXEC pipe failed: … Bad file descriptor` — so the `EBADF` was a
**second, independent** bug that the child-SIGSEGV had been masking all along.

Pinning it: the added `read()` diagnostic (`[read-ebadf]`) **never fired**, so
the `EBADF` did not come from `read(2)`. The handshake fd is the
`socketpair(AF_UNIX, SOCK_SEQPACKET)` rust created for child-spawn IPC (§1), and
**libstd reads it with `recvmsg`, not `read`**. Akuma's socket recv/send
syscalls (`sys_recvmsg`/`sys_recvfrom`/`sys_sendmsg`/`sys_sendto`) resolved the
fd via `get_socket_from_fd`, which only matches smoltcp `Socket(idx)` — a
`UnixSocket{rx,tx}` endpoint fell through to `None → EBADF`. So the parent's
`recvmsg` on its socketpair end returned `EBADF`, and libstd turned that into the
panic. (`read`/`write` already handled `UnixSocket`, which is why the
shell-driven `clang hello.c` — plain exec, no recvmsg handshake — always worked.)

**Fix (`src/syscall/net.rs`):** all four socket send/recv syscalls now detect a
`UnixSocket` fd up front (`fd_is_unix_socket`) and route to the backing pipes via
the existing `sys_read`/`sys_write` paths:
`recvfrom`/`recvmsg` → read the `rx` pipe, `sendto`/`sendmsg` → write the `tx`
pipe (`recvmsg`/`sendmsg` operate on the first iovec, sufficient for libstd's
single fixed-size handshake message). On a successful child `exec`, the child's
CLOEXEC close drops the socketpair's writer, so the parent's `recvmsg` reads EOF
(`0`) = "exec succeeded" — exactly what libstd expects.

Guard: `test_socketpair_recv_send_via_socket_syscalls` drives all four syscalls
over a `UnixSocket` pair via `handle_syscall` and asserts data flows both ways
with no `EBADF`.

> Two distinct bugs hid behind one panic message: a multithreaded-`fork`
> address-space miss (§4b′) that killed the child, and — once the child
> survived — a missing `UnixSocket` case in the socket recv/send syscalls (this
> section). Both had to be fixed before the handshake could complete.

---

## 5. Follow-ups flagged during the investigation

Issues surfaced while bringing up the toolchain that were deliberately *not*
fixed in the main push, with the rationale and current status. With rustc now
working end-to-end, the only genuinely open item is **5b (fork performance)**;
the rest are resolved, intentional, or minor.

### 5a. `SIGSEGV` during the actual link (`cc`/`ld` invocation) — RESOLVED

**Status: resolved.** This was the open question of whether the §3a `WILD-DA`
would reproduce once the spawn `fork` worked. It did **not** — the `WILD-DA`
*was* the multithreaded-fork bug (§4b′), and with that fixed the link runs to
completion: `clang-21`→`ld` produces a working binary (verified 2026-05-31).
There is no separate link-stage SIGSEGV.

### 5b. `fork` memory-copy: performance (top open item) + robustness

**Status: open — the main thing left.** Correct but slow. An end-to-end rustc
compile takes ~3 min, dominated by the `fork` CoW: each `fork` of multithreaded
rustc shares/demotes the whole ~75k-page address space (~30 s `mmap` + ~32 s
`munmap` per the PSTATS). Two distinct follow-ups, somewhat opposed:

- **Performance (priority):** a child that immediately `exec`s doesn't need a
  full address-space copy at all. A vfork-style fast path (share, suspend the
  parent thread, throw the copy away on `exec`) or coarser CoW (share whole
  page-table subtrees by bumping refcounts on L1/L2 tables instead of per-page)
  would cut the per-`fork` cost dramatically.
- **Robustness (secondary):** §4b′ enumerates *tracked regions* (lazy-by-`tgid`
  + sibling threads' eager `mmap_regions`), so it still misses any present page
  no region tracks (internal TLS / `process_info` pages). None are implicated in
  the rustc path, but a page-table walk that shares **every present user page**
  would make `fork` independent of region bookkeeping.

Both rewrite a critical path (a bug here breaks *all* process spawning), so they
were deferred until the toolchain worked — which it now does.

### 5c. Eager-copy (non-CoW) `fork` path only half-updated

**Status: inert path, noted for consistency.** `fork_process` has a legacy
eager-copy branch (`else` of `config().cow_fork_enabled`). Its lazy lookup was
switched to `tgid`, but the sibling-`mmap_regions` union (§4b′ part 2) was **not**
added there. The CoW path is the active one (`[FORK-COW] shared … pages` appears
in every trace), so the eager path is currently dead code — but if it is ever
re-enabled it carries the same sibling-stack bug.

### 5d. Child `mmap_regions` *metadata* for sibling ranges

**Status: moot for the spawn, minor otherwise.** The fork fix CoW-shares sibling
threads' *pages* into the child but does not add those ranges to the child's
`mmap_regions` *metadata*. A libstd fork child execs immediately (which clears
the table), so it does not matter here; a forked child that *doesn't* exec would
have incomplete `munmap`/`mremap` bookkeeping for inherited sibling ranges.

### 5e. `readlinkat` EINVAL flood — intentionally unchanged

**Status: working as intended (no fix needed).** ~337 `readlinkat` calls return
`EINVAL` during a rustc run; this is POSIX-correct (`readlink` on a non-symlink
*is* `EINVAL`) and rustc tolerates it. Listed only so it is not mistaken for a
defect on a future log read. See §4a.

### 5f. `SEQPACKET` socketpair approximation

**Status: pre-existing, acceptable.** The `socketpair` shim (§1) backs an
`AF_UNIX`/`SOCK_SEQPACKET` pair with two byte-stream pipes, which does not
preserve message boundaries. Sufficient for libstd's fixed-size errno handshake;
not a conformant SEQPACKET. Unchanged.

---

## References

- Syscall dispatch + errno consts (`ESPIPE`): `src/syscall/mod.rs`
- socketpair handler: `src/syscall/net.rs`
- Pipe backing: `src/syscall/pipe.rs`
- `read()` EBADF diagnostic, `sys_lseek` (ESPIPE), `sys_readlinkat`: `src/syscall/fs.rs`
- **`UnixSocket` routing in socket recv/send** (`fd_is_unix_socket`, `sys_recvmsg`/`sys_recvfrom`/`sys_sendmsg`/`sys_sendto`): `src/syscall/net.rs`; guard `test_socketpair_recv_send_via_socket_syscalls` in `src/process_tests.rs`
- `do_execve` CLOEXEC-on-success ordering: `src/syscall/proc.rs`
- **Multithreaded-fork lazy-region `tgid` fix**: `fork_process` in `crates/akuma-exec/src/process/mod.rs` (lazy regions keyed by `parent.tgid`, not `parent.pid`)
- mmap region/pid attribution: `src/syscall/mem.rs` (`push_lazy_region(proc.tgid, …)`), `clone_thread` in `crates/akuma-exec/src/process/mod.rs`
- FD table / fork: `crates/akuma-exec/src/process/{types,fd}.rs`, `replace_image` in `crates/akuma-exec/src/process/image.rs`
- Self-tests: `src/process_tests.rs` (`test_lseek_nonseekable_returns_espipe`, `test_failed_exec_preserves_cloexec_fds`, `test_pipe_clone_ref_then_double_close`)
- Related: `docs/APK_MISSING_SYSCALLS.md`, `docs/FORK_MMAP_AND_WAIT_STATUS_FIX.md`

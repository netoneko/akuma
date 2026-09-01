# Linux ABI compatibility

Current-state contract for how the kernel's syscall surface and ELF loader
match the Linux AArch64 userspace ABI. For the userspace libc layer, see
[`musl.md`](musl.md); for per-family syscall detail, see
[`../subsystems/syscalls.md`](../subsystems/syscalls.md).

> **Stability: A (stable).** The "missing syscalls" cohort flared Mar–May 2026
> (Go/Bun/dash/git bring-up) and has been quiet since — those problems are
> resolved. The dispatch model and the errno/auxv/ELF contracts below are
> settled. The recurring lesson: **a syscall that returned `!0u64` (`-1` =
> EPERM) instead of `-errno` silently masks the real failure** — every error
> path goes through `neg_errno`.

## Calling convention

Linux AArch64 ABI. Syscall number in **x8**, args in **x0–x5**, return in x0.

- Entry: EL0 sync exception → `crates/akuma-exceptions/src/lib.rs` → `handle_syscall`
  (`src/syscall/mod.rs:582`).
- **Rump interception first:** `rump_proxy::intercept_box_syscall`
  (`mod.rs:650`) forwards socket-family syscalls for rump boxes (AF_UNIX
  socketpair, nr 199, always native).
- **Native dispatch:** big `match syscall_num` (`mod.rs:656`). Unknown →
  `ENOSYS` (-38) + `[ENOSYS] nr=NNN` log line.

## errno encoding

On failure, **x0 = `-(errno)`** as an unsigned bit pattern (same convention as
Linux). The central helper is `neg_errno(i32) -> u64` (`src/syscall/mod.rs`).

- `!0u64` (`-1`) decodes as `EPERM` and **masks the real failure** — never
  return it from an error path. It survives only where it denotes non-error
  semantics (e.g. `RLIM_INFINITY` in prlimit) or sentinel tracing values.
- `EFAULT` (-14) is returned for pointers failing `validate_user_ptr` /
  `copy_from_user_str` / `copy_from_user_safe`.
- `ENOSYS` (-38) is returned for every undispatched number — decode the log
  line against the asm-generic table.

### Two `Result<_, u64>` families, opposite signs

**The sign is not uniform across the crate boundary, and getting it wrong is
silent.** Two families of fallible helper meet in `src/syscall/`:

| helper | lives in | `Err` carries |
|---|---|---|
| `copy_from_user_str`, `copy_from_user_byte` | `src/syscall/mod.rs` | **negated** (`-14`) |
| `copy_from_user`, `copy_to_user`, `read_user_into`, `write_user_val`, … | `akuma_exec::mmu::user_access` | **positive** (`14`) |

The second is deliberate and documented at its definition — *"`x0 = -errno`
happens at the syscall boundary, not here"* — because that crate is used off the
syscall path too.

Since 2026-08-28 a syscall arm may return `syscall::SysResult`
(`Result<u64, u64>`, `Err` = negated) and use `?`. That makes the mismatch
reachable: `read_user_into(&mut v, p)?` compiles and returns `Err(14)`, which
userspace decodes as **a syscall that succeeded and returned 14** — a wrong
answer, not a fault. Every call site instead spells it:

```rust
if read_user_into(&mut v, p).is_err() {
    return Err(EFAULT);          // this module's EFAULT: negated
}
```

`scripts/check_errno_sign.py` (pre-commit) fails the build on the `?` form.
Audited 2026-08-28: zero violations at the time `SysResult` was introduced. The
`flat` helper in `src/syscall/mod.rs` carries the same explanation next to the
code.

## Pointer validation

`validate_user_ptr` + `copy_from_user_str` + `copy_from_user_safe` /
`copy_to_user_safe` (`crates/akuma-exec/src/mmu/user_access.rs`) bound every
userspace pointer:

- Lower bound `0x1000` (the process info page; NULL/garbage rejected).
- Upper bound is the **dynamic** `user_va_limit()` — it reads the current
  process's `stack_top` (up to 4 GB for large/PIE binaries), falling back to
  `0x4000_0000` when no process is active. The old hardcoded 1 GB cap is gone.

## Syscall numbering

asm-generic (AArch64) table — there is no `syscalls.h` translation layer.
Authoritative constants: `pub const …` block in `src/syscall/mod.rs:177+`.

| Family | Notable numbers |
|---|---|
| fs | `READ` 63, `WRITE` 64, `READV` 65, `WRITEV` 66, `OPENAT` 56, `CLOSE` 57, `LSEEK` 62, `FSTAT` 80, `NEWFSTATAT` 79, `DUP` 23, `FCNTL` 25, `GETCWD` 17, `FACCESSAT` 48 |
| proc | `EXIT` 93, `EXIT_GROUP` 94, `WAITID` 95, `CLONE` 220, `EXECVE` 221, `MMAP` 222, `GETPID` 172, `KILL` (Linux 129) |
| signal | `RT_SIGACTION` 134, `RT_SIGRETURN` 139, `RT_SIGPROCMASK` 135, `RT_SIGSUSPEND` 133 |
| sync | `FUTEX` 98, `MEMBARRIER` 283 |
| poll | `PPOLL` 73, epoll_create1 20 / `epoll_ctl` 21 / `epoll_pwait` 22 |
| time | `CLOCK_GETTIME` 113, `NANOSLEEP` 101 |
| term | `IOCTL` 29 |
| net | `SOCKET` 198, `SOCKETPAIR` 199, `BIND` 200 … `RECVMSG` 212 |

**Akuma-private numbers** (300+) do not exist on Linux and are not part of the
compat surface: `RESOLVE_HOST` 300, `SPAWN` 301, `KILL` 302, `WAITPID` 303,
`TIME` 305, `CHDIR` (also aliased to Linux 49), rich-terminal 307–313,
`GET_CPU_STATS` 314, `SPAWN_EXT` 315, container 316–327. musl-linked binaries
never call these; they exist for the libakuma runtime and the in-kernel shell.

## ELF loading

Two loaders, picked by size (`src/elf_loader.rs` +
`crates/akuma-exec/src/elf/mod.rs`):

- **Buffered path** (`load_elf_with_stack`) — for binaries < 16 MB (the ext2
  read safety cap). Uses the `elf` crate parser; the whole file is slurped
  into kernel heap then mapped.
- **On-demand path** (`load_elf_from_path`) — for binaries ≥ 16 MB (bun/node).
  Manually parses the 64-byte ELF64 header + program headers, then reads each
  PT_LOAD segment **one 4 KB page at a time** via `vfs::read_at()`. Peak
  kernel heap ~4 KB regardless of binary size. Fills inter-segment gaps with
  zero pages. Limitation: no kernel-side SHT_RELA processing for non-PIE
  ET_EXEC (all modern large binaries are ET_DYN and self-relocate).

`spawn_process_with_channel_ext` and `do_execve` try `read_file()` first; on
`FsError` they fall back to the on-demand path via `file_size(path)`.

### Dynamic linking (PT_INTERP)

- A `PT_INTERP` program header names the interpreter; the loader reads it via
  `read_file()` (interpreter is small, always < 16 MB) and maps it at
  `interp_base = 0x3000_0000` (`crates/akuma-exec/src/process/mod.rs:1505`).
- fork/CoW shares the whole 2 MB interp window (`cow_share_range(... "interp")`).
- AT_BASE in the auxv carries the interpreter's load base to userspace.

### auxv

Built in `crates/akuma-exec/src/elf/mod.rs` (`setup_linux_stack`). Entries:
`AT_PHDR`, `AT_PHNUM`, `AT_PHENT`, `AT_PAGESZ` (4096), `AT_ENTRY`,
`AT_CLKTCK` (100), `AT_RANDOM` (16 random bytes), `AT_UID`/`AT_EUID`/
`AT_GID`/`AT_EGID` (all 0 — single-user), `AT_HWCAP` (`AARCH64_HWCAP`),
`AT_HWCAP2` (0), and `AT_BASE` when an interpreter was loaded.

## execve

`do_execve` (`src/syscall/proc.rs`) calls `Process::replace_image[_from_path]`
(`crates/akuma-exec/src/process/image.rs`) — a true in-place image replacement
(preserves PID + open fds; strips O_CLOEXEC fds), not spawn-as-exec. See
[`../subsystems/syscalls/proc.md`](../subsystems/syscalls/proc.md).

## Process info page

`PROCESS_INFO_ADDR = 0x1000` (`crates/akuma-exec/src/process/types.rs:45`):
kernel-written, read-only to EL0. Carries pid, ppid, argc, and the CWD string
(`ProcessInfo.cwd_data`, 256 B). CWD is read by libakuma's `getcwd`; see
[`../subsystems/vfs.md`](../subsystems/vfs.md) and `archive/CWD.md`.

**Fork invariant:** CoW must re-map `PROCESS_INFO_ADDR` to the child's own
frame **after** `cow_share_range` — Go ARM64 binaries have `code_start =
0x1000`, so the parent's PTE for 0x1000 is otherwise shared into the child
(see `archive/GO_FORK_EXEC_FIXES.md` bug 1).

## Shared fd tables (CLONE_FILES)

`Arc<SharedFdTable>` (`crates/akuma-exec/src/process/mod.rs`) holds the
`BTreeMap<u32, FileDescriptor>` + CLOEXEC/nonblock sets + atomic `next_fd`.

- **clone_thread (CLONE_VM):** `Arc::clone` — same table, all threads see new
  fds/closes immediately.
- **fork:** `clone_deep_for_fork` — deep copy, bumps pipe refcounts, **strips
  EpollFd entries** (epoll instances are not refcounted; sharing via dup across
  fork is an open wedge — see [`../../runbooks/debug-network.md`](../../runbooks/debug-network.md)
  epoll section).

See `archive/SHARED_FD_TABLES.md` and [`../subsystems/vfs.md`](../subsystems/vfs.md)
for the `FileDescriptor` enum.

## Argument size limit

`MAX_ARG_STRLEN` (`src/config.rs:147`): 128 KB release / 8 KB size / 4 KB
extreme — the per-argument cap (Linux's value is 128 KB). The Go forktest
128 KB fix is a notable regression guard.

## Background

- `archive/SYSCALL_HARDENING.md`, `archive/SYSCALL_ERRNO_COMPLIANCE_CHANGES.md`
  — the `-errno` compliance pass (replaced the `!0u64` / EPERM-masking
  convention).
- `archive/ON_DEMAND_ELF_LOADER.md` — on-demand ELF loader + dynamic VA space
  + `user_va_limit()` pointer-validation fix.
- `archive/PROPER_EXECVE_PLAN.md` — the spawn-as-exec → true-execve transition.
- `archive/SHARED_FD_TABLES.md`, `archive/SPLIT_SYSCALLS.md`.
- `userspace/libakuma/docs/SYSCALLS.md` — userspace syscall wrapper docs.

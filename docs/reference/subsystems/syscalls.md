# Syscalls / Linux ABI

Current-state architecture for syscall dispatch, the `sc-*` feature gates, and
Linux compatibility.

> **Stability: A (stable) for dispatch.** The "missing syscalls" cohort flared
> in Mar–May (Go/Bun/dash/git bring-up) and has been quiet since — those
> problems are resolved. The dispatch model (`handle_syscall` + rump
> interception) is settled. errno compliance is tracked in
> `archive/SYSCALL_ERRNO_COMPLIANCE_CHANGES.md`. **Per-family grades vary** —
> `mem`/`net`/`signal`/`sync` are graded C (active risk, touched in the Jun
> 2026 memory+signal crisis); see the table below before trusting a specific
> family.

For memory syscalls see [`memory.md`](memory.md); for network syscalls see
[`networking.md`](networking.md) + [`rump-stack.md`](rump-stack.md).

## Dispatch

Linux-compatible ABI: syscall number in **x8**, args in **x0–x5**, return in
x0. Entry: EL0 sync exception → `crates/akuma-exceptions/src/lib.rs` → `handle_syscall`
(`src/syscall/mod.rs:582`).

`handle_syscall` flow:
1. Store `syscall_num` on the thread + process (`current_syscall`, `last_syscall`) — this is what `ps` prints.
2. Optional `SYSCALL_DEBUG_IO_ENABLED` tracing.
3. **Rump interception first:** `rump_proxy::intercept_box_syscall(syscall_num, args)` (`mod.rs:650`). If the current process is in a rump box and the syscall is socket-family (or operates on a rump-owned fd), it is forwarded to the box's `rump_server`. AF_UNIX socketpairs (nr 199) are excluded — always native.
4. **Native dispatch:** a big `match syscall_num` (`mod.rs:656`). Unknown → `ENOSYS` (-38) + `[ENOSYS] nr=NNN` log line (decode against the asm-generic table).

## `src/syscall/` forbids `unsafe`

`src/syscall/mod.rs` carries `#![forbid(unsafe_code)]` (2026-08-31), which
applies to every submodule in the table below. It is the first enforced ban
outside `crates/` — a bin crate cannot take one whole (trap-frame and
page-table work is the job in `src/main.rs`'s boot entry), but this is the
subtree that runs with **userspace-controlled arguments on every call**. The
exception path that used to hold 87 of the bin crate's sites left for
`akuma-exceptions` on 2026-09-01, taking `src/` production `unsafe` to 11
sites in 3 files — the claim about bin crates is unchanged, its scale is not.

**If you are adding a syscall and need a raw pointer, a page-table edit, or a
system-register write, do not reach for `#[allow(unsafe_code)]` — `forbid`
refuses it, deliberately.** Put the operation behind a named function in the
crate that owns the thing it pokes, and state the obligation there. What already
exists, and is very likely what you want:

| you need to | call |
|---|---|
| read/write user memory | `copy_from_user` / `copy_to_user` / `write_user_val` / `read_user_into` — the folded API, re-exported from `mod.rs` |
| read a NUL-terminated user string of unknown length | `copy_from_user_str`, or `read_user_byte` for one byte |
| map a page into the caller's address space | `Process::with_address_space` + `UserAddressSpace::map_user_page_tracked{,_no_flush}` — it tracks the frames and refuses a VA that is not this address space's |
| read or write a physical frame's bytes | `akuma_mmu::copy_from_phys` / `copy_to_phys` — bounds-checked against PMM-managed RAM |
| set userspace's TLS base | `akuma_cpu::sysreg::set_tpidr_el0` |
| eret into a user context | `akuma_exec::process::enter_user_mode_checked` |

The ban means no `unsafe` is written here; it does not mean the layer is proven
sound. One of the moved wrappers — `with_own_process_exclusive`, used by
`execve`'s destructive window — discharges two of its three safety clauses and
rests on staying the single enumerated call site for the third. **Adding a second
caller of it is a change to that argument, not ordinary use.** Full accounting:
[`archive/SYSCALL_UNSAFE_CLEANUP.md`](../../archive/SYSCALL_UNSAFE_CLEANUP.md).

## The `src/syscall/` split

`src/syscall/mod.rs` is the dispatcher; per-family logic lives in submodules.
Each gated by a `sc-*` feature (default-on; minimal builds re-add selectively).
Each family now has its own current-state doc under
[`syscalls/`](syscalls/) — grades vary per family (a quiet family living next
to an actively-churning one doesn't inherit its risk).

| Submodule | Family | Gate | Doc | Grade |
|---|---|---|---|---|
| `fs.rs` | open/read/write/stat/getdents/... | always | [`syscalls/fs.md`](syscalls/fs.md) | A |
| `mem.rs` | mmap/munmap/brk/mremap/membarrier | always | [`syscalls/mem.md`](syscalls/mem.md) | **C** |
| `net.rs` | socket/connect/bind/listen/sendto/recvfrom | always (smoltcp **or** rump-routed) | [`syscalls/net.md`](syscalls/net.md) | **C** |
| `pipe.rs` | pipe/fifo | always | [`syscalls/pipe.md`](syscalls/pipe.md) | A |
| `poll.rs` | poll/ppoll/epoll | `sc-epoll` (Tier 2) | [`syscalls/poll.md`](syscalls/poll.md) | B |
| `proc.rs` | fork/clone/execve/wait/exit | always | [`syscalls/proc.md`](syscalls/proc.md) | A |
| `signal.rs` | rt_sigaction/kill/tkill/sigreturn | always | [`syscalls/signal.md`](syscalls/signal.md) | **C** |
| `sync.rs` | futex | always | [`syscalls/sync.md`](syscalls/sync.md) | **C** |
| `term.rs` | ioctl (TIOCGWINSZ/TIOCSWINSZ) + rich terminal 307–313 | always | [`syscalls/term.md`](syscalls/term.md) | B |
| `time.rs` | clock_gettime/nanosleep | always | [`syscalls/time.md`](syscalls/time.md) | A |
| `log.rs` | kernel log (dmesg) | always | [`syscalls/log.md`](syscalls/log.md) | A |
| `aio.rs` | io_setup/io_submit/... | `sc-aio` | [`syscalls/aio.md`](syscalls/aio.md) | B |
| `container.rs` | box/join_box/core_init | `sc-containers` | [`syscalls/container.md`](syscalls/container.md) | B |
| `eventfd.rs` | eventfd | `sc-eventfd` (Tier 2) | [`syscalls/eventfd.md`](syscalls/eventfd.md) | B |
| `msgqueue.rs` | SysV msg queues | `sc-sysv-ipc` | [`syscalls/msgqueue.md`](syscalls/msgqueue.md) | A |
| `pidfd.rs` | pidfd_open/waitid | `sc-pidfd` (Tier 2) | [`syscalls/pidfd.md`](syscalls/pidfd.md) | A |
| `timerfd.rs` | timerfd_create/settime | `sc-timerfd` | [`syscalls/timerfd.md`](syscalls/timerfd.md) | B |

## Feature gates & ExecRuntime stubs

The `sc-*` features are compile-time gates. **Tier 1** (`sc-aio, sc-sysv-ipc,
sc-containers, sc-timerfd`) are pure dead weight when off —
nothing else references them. **Tier 2** (`sc-eventfd, sc-pidfd, sc-epoll`)
each need a no-op `ExecRuntime` callback stub when off (e.g.
`eventfd_close: noop_u32`, `epoll_destroy: noop_u32` — `src/main.rs:412,451`).

When adding a new syscall family: add a `sc-<name>` feature in `Cargo.toml`,
gate the submodule, and (if Tier 2) add the no-op stub + keep
`scripts/build_devbox.sh` and `overlays/devbox/run.sh` feature lists in sync.
See [`../../runbooks/add-syscall-feature.md`](../../runbooks/add-syscall-feature.md).

## Linux ABI compatibility

- **Syscall numbers:** asm-generic (aarch64) table. An `[ENOSYS] nr=NNN` log
  line means that number isn't dispatched — decode it against the table.
- **errno compliance:** negative return values are `-errno`. Tracked in
  `archive/SYSCALL_ERRNO_COMPLIANCE_CHANGES.md` + `archive/SYSCALL_HARDENING.md`.
- **Where the values come from (one table, since 2026-08-14):**
  `akuma_primitives::errno`. Use the pre-negated `errno::negated::*` form when
  returning from a syscall arm — the `src/syscall/` modules already have them in
  scope through `use super::*` — and the positive form for an error carried inside
  the kernel (a `Result<_, i32>`), negated once at the boundary with
  `neg_errno()`. `akuma_net::socket::libc_errno` is an alias of the same table.
  **Do not write `(-22i64) as u64` or `i64::from(-libc_errno::EINVAL) as u64`**:
  both spellings existed here, across five tables, and one of them had drifted
  from its own comment (`archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §5.7).
  A name missing from the table is added there, not at the call site.
- **Copying to and from user memory (one helper, since 2026-08-14):**
  `akuma_exec::mmu::user_access` — `copy_to_user(dst_user, &[u8])`,
  `copy_from_user(&mut [u8], src_user)`, `write_user_val(dst_user, &T)`,
  `read_user_into(&mut T, src_user)`, plus `as_user_bytes{,_mut}` for arrays of ABI
  structs. All safe `fn`s: they range-check, demand-page the range, then copy, so
  **a separate `validate_user_ptr` call is no longer needed** at a site that only
  copies. The syscall modules see them through `use super::*`.
  - **Inside a lock, or in an exception handler:** use the `*_with(..., Prefault::No)`
    forms. Prefaulting allocates frames, takes `as_lock` and can read a file, none
    of which may happen under an IRQ-masked spinlock or on the fault path.
  - `validate_user_ptr` still exists and is still right when the check must happen
    *before* something else — an allocation sized by the caller, a lock, an fd
    allocation, or a blocking wait. `UNSAFE_AUDIT.md` §4.0 lists every surviving
    call and which of those three reasons it is there for.
  - The raw `copy_{to,from}_user_safe` are the byte loop plus a fault trampoline and
    check **nothing**; the only deliberate caller left is `copy_from_user_byte`
    (NUL-terminated strings have no range to validate up front).
  - **The range check tests the leaf PTE's AP bits, not mere presence** (since
    2026-08-14). Kernel RAM is identity-mapped EL1-only into every user address
    space, so a kernel VA *is* present in TTBR0 and used to pass — with nothing to
    stop the copy, because the byte loop runs at EL1. `is_current_user_range_mapped`
    now requires AP bit 6 (`AP_RW_ALL`/`AP_RO_ALL`), i.e. reachable from EL0. Two
    consequences: a kernel VA as a syscall buffer returns `EFAULT`, and so does a
    `PROT_NONE` page — as on Linux. A read-only user page still passes, deliberately:
    an EL1 write to one is how a CoW break is triggered.
    `archive/USER_COPY_FOLD.md` §7; boot test `kernel_va_rejected_as_user_pointer`.
  - `is_current_user_**page**_mapped` is the other question — plain presence — and
    stays that way: its callers are demand-paging and teardown paths asking "has
    this VA been filled in yet", where a `PROT_NONE` guard must read as present.
- **musl compatibility:** `archive/MUSL_COMPATIBILITY.md`. musl is the userspace
  libc; the kernel aims to run unmodified musl-linked binaries.
- **`MAX_ARG_STRLEN`:** 128 KB release / 8 KB size / 4 KB extreme (`config.rs:147`). The Go forktest 128 KB fix is a notable regression guard.

## Blocking vs non-blocking

Syscalls that would block (read on empty pipe, waitpid, poll) follow the
blocking pattern in `archive/SYSCALL_BLOCKING.md`: register a `Waker` on a wait
queue, then `schedule_blocking()`. The producer fires the waker. See
[`scheduler.md`](scheduler.md) "Blocking & wait/wake".

**`SYSCALL_BLOCKING` rule:** never block inside a preemption-disabled closure.

## Porting a new binary (missing syscalls)

When a binary fails with `[ENOSYS] nr=NNN`:
1. Decode `NNN` against the asm-generic table.
2. Check `archive/<BINARY>_MISSING_SYSCALLS.md` — the per-binary porting notes
   (Go, Bun, Node, git, apk, curl, dash, xbps, crush). The whole cohort is
   resolved history.
3. Common gaps that bit many binaries: `socketpair` (199) for Rust std subprocess
   spawn; `fcntl(F_SETFD)` for c-ares DNS; `getrandom`; `ppoll`/`epoll`.

## Background

- `archive/SPLIT_SYSCALLS.md` — the split into `src/syscall/`.
- `archive/SYSCALL_HARDENING.md`, `archive/SYSCALL_ERRNO_COMPLIANCE_CHANGES.md`.
- `archive/MUSL_COMPATIBILITY.md`, `archive/TERMINAL_SYSCALLS.md`.
- `userspace/libakuma/docs/SYSCALLS.md` — the userspace syscall wrapper docs.

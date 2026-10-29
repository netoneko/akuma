# Rust toolchain coverage — what `rustc` needs vs what this kernel serves

**Stability: C** (the table moves with every ladder stage; re-score against
[`syscalls.md`](syscalls.md) before relying on a row).

> **Corrected by measurement, 2026-10-29 (session pn).** The scores below are an
> *inventory* of two archive lists, not a measurement of what `rustc` does. The
> measured `rustc --version` census — see [`rust-syscalls.md`](rust-syscalls.md) and
> `PLAN-pn.md` — shows `rustc 1.100` needs a **thread at startup** (the ctrlc
> handler's `clone`), which neither list contains; the run also demands `sigaltstack`,
> `membarrier`, legacy `readlink`(89), a real `mprotect`, and a ≥1 GiB mmap slab
> before the first `clone`. The percentages here understate the gap; the current-state
> list and order of work live in `rust-syscalls.md` / `PLAN-pn.md`.

Two archive docs record what the **full** kernel needed to run Rust, and they
are the stage-9–12 shopping list for this kernel — rustc's behaviour does not
change between kernels, so the syscalls it reaches for do not either:

- `docs/archive/RUST_TOOLCHAIN.md` — `rustc hello.rs` end-to-end (2026-05-31):
  the spawn-handshake list.
- `docs/archive/AKUMA_SELF_HOSTING.md` — `cargo build -p akuma` in-VM
  (2026-06-19): what a real cargo build needs on top — threads, futex, the
  linker's `MAP_SHARED` writeback, long argv.

Both lists are scored below against [`syscalls.md`](syscalls.md).

## List 1 — the `hello.rs` bring-up (RUST_TOOLCHAIN.md): **6 of 10 served (60%)**

| # | item (section) | why rustc needs it | here |
|---|---|---|---|
| 1 | `socketpair` (§1) | the libstd spawn handshake: parent and child exchange the exec-errno over a CLOEXEC pair | **missing** — no AF_UNIX at all; the number prints `unhandled` and answers ENOSYS |
| 2 | `recvmsg` (§4d) | the parent's read of that handshake | **missing** — same reason |
| 3 | `sendmsg` (§4d) | the child's write of it | **missing** — same reason |
| 4 | `recvfrom` / `sendto` (§4d) | the datagram forms of the same verbs | served (AF_INET only: UDP source/destination handled; there is no UnixSocket routing to reproduce) |
| 5 | `lseek` → `ESPIPE` on non-seekable (§4a) | the linker probes seekability; EINVAL-class answers are fatal | served — the fd table decides by kind, so a socket/pipe answers `ESPIPE`, an unbound fd `EBADF` |
| 6 | `readlinkat` (§4a) | `/proc/self/exe` and toolchain path resolution | served (raw ext2 walk, lstat-style) |
| 7 | `fork` replicating the whole address space (§4b′) | rustc forks the linker out of itself | served for the case this kernel has: the real fork copies the whole user space eagerly (no CoW — no page-fault handler exists); the multithreaded clause is list 2's problem |
| 8 | `execve` with CLOEXEC close-on-success ordering (§4c) | handshake fds must survive a failed exec and die only on a good one | served — both crossings save the fd table, load the image, restore on failure, sweep cloexec on success (child.rs) |
| 9 | `close`/`dup`/`dup3` fd-kind routing (§1) | the handshake pair is dup'd and closed through the generic paths | served — routing is table-first by kind ([`syscalls.md`](syscalls.md) § fd routing) |
| 10 | `close_range` (§1) | libstd closes everything above fd 2 around the spawn | **missing** |

## List 2 — the full-kernel self-host build (AKUMA_SELF_HOSTING.md): **1 of 10 served**

`cargo build` is a different animal from `rustc hello.rs`: cargo is
multithreaded, rustc runs rayon workers, and `rust-lld` writes its output
through a shared mapping. Rows marked **N/A** name something this kernel does
not have at all — they become real the moment the row above them lands.

| # | item (section) | why the build needs it | here |
|---|---|---|---|
| 1 | threads — `CLONE_THREAD` clone, thread groups, tgid (§7g/§7h) | rustc's rayon codegen workers; cargo's jobserver | **missing** — no threads exist; `clone` is the M2 blocking arm with a three-flag whitelist |
| 2 | `futex` — WAIT/WAKE, `FUTEX_WAIT_BITSET` with absolute deadlines (§7d, §7g) | how those workers park | **missing** — no futex arm; the parking that exists (wait4, pipes) is slot-based |
| 3 | `exit_group` reaping siblings before notifying the parent (§7h) | a rayon leader's exit must terminate+wake its workers before `wait4` reaps it, or the build wedges at proc-macro2 | **N/A** — no thread groups; `exit_group` serves the one task. The ordering rule lands with row 1 |
| 4 | writable `MAP_SHARED` file `mmap` + writeback on `munmap`/`msync`/exit (§7d) | `rust-lld`'s `FileOutputBuffer` — without writeback the linked binary is **0 bytes** and cargo reports success | **missing** — `mmap` is anon + `MAP_PRIVATE` fill only, and `munmap` is a deliberate no-op; this is a real memory-model gap, not a flag arm |
| 5 | `msync` (§7d) | the POSIX flush point lld uses | **missing** — no arm (ENOSYS); lands with row 4 |
| 6 | 128 KB argv strings + `E2BIG` (§7i; Linux `MAX_ARG_STRLEN`) | smoltcp's build script passes one ~5 KB `--check-cfg` arg | **missing** — the read-back budget is 16 pointers × 256 B (`ARGV_MAX`/`STR_MAX`, child.rs) with no `E2BIG`; a long arg is silently cut |
| 7 | `getpriority`/`setpriority` (§7j) | rustc's threadpool calls it opportunistically; an ENOSYS return was used as a pointer and SIGSEGV'd the build unit | **missing** — same crash shape here: `unhandled` + `-ENOSYS` in rax |
| 8 | a real CWD / `chdir` (§7, §7e) | cargo resolves `.cargo/config.toml` and relative manifest paths against it | **missing** — no working directory exists (`getcwd` is always `/`); the full kernel's workaround was env-var invocation, available here too |
| 9 | signals — `tkill`/`tgkill`, dispositions, `rt_sigsuspend` (§7k.3/§7k.5) | the build storms ~10,400 `tkill(SIGUSR1)`; musl pthreads need RT signals for cancellation | **missing** — pinned exclusion (`rt_sigaction`/`rt_sigprocmask` are no-ops); ENOSYS is survivable only because the callers ignore the return |
| 10 | `renameat` (§7d) | lld `munmap`s + renames its output into place | served |

Not scored (kernel-internal or machine prerequisites, not syscalls): the
AArch64 D-cache/I-cache coherency fix (§7j — amd64 is snooped, the bug class
does not exist here), the 6→16 GB RAM story (§3), disk staging (§2), fs-cache
(§7c — a performance feature).

## What the missing items actually are

Two subsystems and a handful of arms, in the order a stage hits them:

1. **AF_UNIX** — `socketpair`, `recvmsg`, `sendmsg`, then `close_range`. The
   libstd spawn handshake is the first thing any rustc invocation does; it
   fails before codegen. `crates/akuma-net-unix` is the state machine,
   already shared — this kernel's net glue is AF_INET-only and does not pull
   it. Route by fd kind (the §4d lesson), with the pipe pair backing it
   ([`pipes.md`](pipes.md) already owns the refcount rule).
2. **Threads + futex** — `CLONE_THREAD`, thread groups, the futex family, and
   the §7h `exit_group` ordering. `rustc` proper is multithreaded; list-1
   row 7's eager fork is not a substitute. This is the deep blocker for
   stages 10–12, and it is exactly the SMP slice the ladder has not reached.
3. **The linker's memory contract** — writable `MAP_SHARED` with writeback on
   `munmap`/`msync`/exit. Without it the link *succeeds* and produces a
   0-byte binary (§7d's trap). This one conflicts with a pinned exclusion
   (`munmap` is a no-op because slabs outlive the task), so it lands as a
   real memory-model change, not an arm.
4. **Small arms** — `getpriority`/`setpriority`, the 128 KB argv budget with
   `E2BIG`, `close_range`, `chdir`. Each is a day, not a design.

Signals (list-2 row 9) are deliberately last: every consumer tolerates their
absence today, and the pinned exclusion stays until a stage genuinely needs
delivery.

## What this kernel gets for free

The archive's hardest bugs are absent here by construction: the
multithreaded-`fork` sibling-stack loss (§4b′) needs threads this kernel does
not have; the CLOEXEC-pipe `EBADF` (§4d) was a missing fd-kind route, and
this kernel's dispatch is table-first from day one; the §7k.4/§7k.6 stale
I-cache corruption is an AArch64 incoherency an snooped amd64 cannot have;
the eager-fork performance problem (§5b) is the *only* fork here, one 4 KiB
copy per page with no CoW bookkeeping.

## Background

`docs/archive/RUST_TOOLCHAIN.md` (list 1), `docs/archive/AKUMA_SELF_HOSTING.md`
(list 2), [`syscalls.md`](syscalls.md) (the table being scored),
[`../gaps.md`](../gaps.md) (the ladder ranking this feeds).

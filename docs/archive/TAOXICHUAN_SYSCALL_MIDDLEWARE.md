# B22: syscall middleware for the minimal kernel (design + first cut, 2026-11-05)

**Status: first cut written, host-tested, and BOOTED in Shanghai on both machines (QEMU TCG, 1 and 4 cores), including a 4-thread contention check on the `own` autolock.** Code:
[`crates/akuma-tokio-kernel-core/src/middleware.rs`](../../crates/akuma-tokio-kernel-core/src/middleware.rs); how to use it:
[`crates/akuma-tokio-kernel-core/docs/middleware.md`](../../crates/akuma-tokio-kernel-core/docs/middleware.md) (the guide, current state);
user-facing text: [`crates/akuma-tokio-kernel/README.md`](../../crates/akuma-tokio-kernel/README.md) § Syscall middleware.
This is the record of the design and of what booting it found; the rules as they stand now are in the guide.
The `middleware` cargo feature is **off by default for every kernel**; with it off none of this is compiled.

## Why

A kernel built on `akuma-tokio-kernel` (one `main.rs`, two machines) had no way to answer a syscall of its own, watch
one, or refuse one, short of editing the machine halves' `dispatch`. B22 gives a kernel author one table of hooks
around dispatch, the same on amd64 and aarch64, without making the common path pay for it.

## Decisions (each was argued in the design session; the reason is the point)

1. **One table, flat, no chaining.** Hooks are plain `fn`s. There is no `next`, no priority, no registration order.
   Two entries for one call in one tier panic at arm time. A second `Kernel::middleware(..)` panics. Someone who
   wants layers writes a function that calls their functions; nothing in the framework makes that feel supported.
   (We are not node developers.)
2. **Tiers, not a pipeline.** `kernel_pre` (may `Pass`, `PassAndForget`, `Return`, `Errno`),
   `fallback` (runs only where the answer is `-ENOSYS`: add a syscall without being able to break one), `kernel_post`
   (may replace the result), and `own_pre`/`own_post` (observe only).
3. **No hook ever touches a lock.** `kernel_*`/`fallback` run inside the big kernel lock, which the kernel takes.
   `own_*` run **outside** it, with the middleware's private state `S` behind an *autolock* the framework owns (a
   spinlock held with preemption off for exactly the hook call, never while the big lock is taken, so there is no
   lock-order question). Which mode a hook is in is its tier; there is no per-hook lock option and no "own lock".
4. **`own` hooks get no user memory.** The software page-table walk behind a user copy races a sibling thread's
   `munmap`/`mprotect` (which run under the big lock) and can read a frame that now belongs to another process.
   Anything that reads the caller's memory is a `kernel` hook.
5. **Cached by number, not by verdict.** A verdict depends on the arguments, so it is never cached. What is cached
   is "does this tier want this number": a 512-bit mask per tier, built when the table is armed, read **once** per
   call and latched (`Latch`) so a pre-hook that prunes itself cannot make its own post-hook vanish (the rule
   `akuma_bkl::policy` already uses). An unhooked call costs a relaxed load and a bit test per tier. A hook may
   return `PassAndForget` to clear its own bit. Numbers from 512 up are always asked; that is where custom calls live
   (`CUSTOM_BASE` = 1000, above everything Linux assigns).
6. **Some calls are observed, never replaced:** `exit`, `exit_group`, `clone`, `execve`, `rt_sigreturn` rewrite the
   frame or do not return. A `fallback` and a changed result for them are ignored (`observe_only`). Narrowed
   2026-11-05: a `kernel_pre` *refusal* skips the call and cannot corrupt the frame, so it is honoured for `execve`
   (`may_refuse_before`; an environment or signature policy needs it) and still ignored for `exit`, `exit_group`,
   `clone` and `rt_sigreturn`.
7. **No `unsafe`, and no raw frames.** The module is `forbid(unsafe_code)` and the API needs none: a hook gets
   `KernelCtx`/`OwnCtx` (number, `Option<Syscall>`, args, tid, pid) and checked user reads/writes
   (`user_read`, `user_write`, `user_read_u64`, `user_str`) over the kernel's own copies. The x86 `SyscallFrame` and the
   aarch64 `Frame` never leave the machine halves. Raw access means patching the attribute out of your own crate.
8. **No runtime off-switch.** Same lesson as `wx=0` (removed 2026-11-05): a production kernel does not get a
   command-line hatch to change what a syscall does.
9. **Hooks cannot mutate the kernel context** (2026-11-05, the owner: "sets a bad precedent"). The first cut let a
   `kernel_pre` rewrite `ctx.args` and the call then saw the new values; nothing used it (one unit test did). It is gone:
   every `kernel` hook takes `&KernelCtx`, `run_kernel` takes the arguments by value and no longer writes them back, and
   the machine halves lost their write-back. Why it matters beyond tidiness: rewriting arguments is how one hook defeats
   another's check (a guard validates a path, a later hook swaps the pointer), and a policy middleware wants the guarantee
   "a hook can only allow, refuse or answer, never change what runs". It also makes hooks pure functions of the context, so
   combining them has no hidden order effects. `OwnCtx` keeps `&mut` for one thing only, the hook's own private `state`.
   If a real need for rewriting appears, add it back as an explicit `Verdict::Rewrite(args)` with a use case in hand.

## Where the seams are

```text
amd64  dispatch():        own_enter -> [bkl_enter] -> dispatch_locked -> dispatch_inner:
                            fold -> kernel tier { kernel_pre -> dispatch_after_fold -> fallback -> kernel_post }
                          -> deliver_at_return -> [bkl_leave] -> own_leave
aarch64 trap::el0_sync(): own_enter -> [smp::enter] -> svc -> syscall::dispatch:
                            kernel tier { kernel_pre -> dispatch_arms -> fallback -> kernel_post }
                          -> at_return -> [smp::leave] -> own_leave
```

`Kernel::middleware(&MW)` stores the table (`&'static dyn Dispatch`, so `Middleware<S>` of any `S` fits); boot arms
and installs it with the machine's `Machine { decode, encode, copy_in, copy_out, pid }` before the first ring-3
entry. The amd64 `dispatch_inner` was split at the fold: everything after it is `dispatch_after_fold`, unchanged.

## What was verified (and what was not)

- **Host:** 11 tests in `middleware.rs` (nothing hooked runs nothing; pre answers and the call does not run; the
  result is replaced and the call is given the original arguments; fallback serves only `-ENOSYS`; the five observe-only calls; `PassAndForget` with the
  latch; `own` state; checked user memory and bounded strings; duplicate refusal; same call in two tiers; numbers
  from 512 always asked). `cargo test -p akuma-tokio-kernel-core --features middleware --target $HOST`.
- **Builds:** `akuma-tokio-kernel` with and without `--features middleware` for `x86_64-unknown-none` and
  `aarch64-unknown-none`; `taoxichuan` with default features for both; clippy clean on the new lines.
- **Booted (2026-11-05, QEMU TCG: amd64 `microvm`, aarch64 `virt`):** `sh shanghai/mw_gate.sh [x86_64|aarch64|all]`
  (`SMP=N`) builds Shanghai twice per architecture (default, and `--features middleware` with the sample table in
  `shanghai/src/middleware.rs`), boots each with `shanghai/mwprobe/mwprobe.rs` as `init`, and judges the probe's own
  result line. The middleware kernel passes every case on both machines (13 with the original table; 26 with the no-dynamic-loading demo and its child probes below; earlier revisions of this record said 12 and 14 because the result line was counted as a case): `fallback` 1000/1001 (a value, a write
  through a user pointer, `EFAULT` on a bad one, an unserved number still `ENOSYS`), `kernel_pre` on `openat` (reads the
  path from user memory, refuses `/mw-deny` with `EACCES`, passes the rest), `kernel_post` on `gettid`, `own_pre`/`own_post`
  on `getpid` (5 calls seen 5 times), and the contention case: 4 threads x 500 `getpid` at once, where the `own_pre` hook
  does a split read-then-write on its private state; all 2000 were seen and the state equals the atomic count (no lost
  update). The control (no middleware) answers 1000 with `ENOSYS` and every touched call is stock. Runs: amd64 at 1 core
  (before the threaded case existed) and at 4; aarch64 at 1 and at 4 (4 CPUs online on both).
- **Added 2026-11-05, the no-dynamic-loading demo** (`shanghai/src/nodl.rs`, three `kernel_pre` guards): `mmap` of a file with
  `PROT_EXEC` is refused with `EACCES`, `mprotect` adding `PROT_EXEC` is refused, and an `execve` whose environment names an
  `LD_*` variable is refused with `EPERM` (the last needed the `execve` narrowing above). `mwprobe` checks each refusal and
  the neighbouring cases that must stay allowed (anonymous `PROT_EXEC` memory, a read-only file mapping,
  `mprotect(PROT_READ)`, an `execve` with a clean environment, which is allowed and whose new image prints the result).
- **Children and real dynamic programs** (same day): the probe forks a child with a raw `clone` (no exec), execs a static child
  (its own pid; the console's `[nodl]` lines carry it), execs with `LD_PRELOAD` (refused, `EPERM`), and runs two real dynamic
  programs from Alpine (`shanghai/mw_fetch_dyn.sh`: musl loader, `busybox`, `xz` + `liblzma.so.5`). **Result, both machines
  at SMP=4: 26 cases on the middleware kernel, 7 on the control, all passing.** `xz` runs on the control kernel and fails under
  the guards inside `ld-musl` (`Error loading shared library liblzma.so.5: Permission denied`). A dynamic executable that needs only
  libc (`busybox`) **still runs**: the kernel maps the interpreter and the main binary without a syscall and `ld-musl` is libc,
  so there is no second mapping to refuse. That case is pinned in the probe as a known gap. It is a demonstration, not a wholesale
  ban. `memfd_create` and shebang scripts were not checked.
- **A feature, off by default** (2026-11-05, the owner): the three guards are behind Shanghai's `no-dynamic-loading` feature (implies
  `middleware`), so the sample table alone does not carry them. The gate boots three kernels per machine (stock; `middleware`
  alone, where the probe asserts the guards are absent; `no-dynamic-loading`, booted with `initargs=nodl`, where it asserts they are
  on), so a build that silently lacks or wrongly has the guards fails.
- **Closing the libc-only gap** needs a hook where the loader handles `PT_INTERP`, not a syscall hook: an `exec_image` tier (path,
  interpreter path, read-only image bytes, may refuse). Not built; also where a signed-binaries check belongs. Noted: running the
  loader directly (`ld-musl <binary>`) has no `PT_INTERP`, so a `PT_INTERP` check alone would not see it (the `mmap` guard
  probably still refuses the binary's own mapping; not tested).
- **The first test subject was wrong:** `bzip2` ran under the guards with no refusal, which looked like a failed ban. Alpine builds
  libbz2 into the binary (`strings` shows only `libc.musl` as a dependency), so it never asked for a second library. `xz` replaced
  it, and `mw_fetch_dyn.sh` now checks the premise (`xz` names `liblzma`, `busybox` needs nothing beyond libc).
- **Found by booting it:** the first real user could not write `..Middleware::new(0)` (private fields), which the
  in-module tests could not catch; the tables are now the all-public `Hooks<S>` (`Middleware::new(state, Hooks {..})`).
  The first sample also hooked `getuid`, which this kernel does not serve without `shell`, and added 1000 to its
  `-ENOSYS`: the sample now only adds to a non-error result.
- **Found by booting aarch64:** `scripts/link_kernel.sh` wrote no `.bin` for any kernel (cargo's link-output path
  changed) and Shanghai's `build.rs` was amd64-only; both fixed, see `docs/archive/LINK_KERNEL_BIN_LAYOUT.md`.
- **NOT done:** the feature-on cost against the syscall floor (`scripts/benchmarks`, run A/B/A). With the
  feature off, amd64's `dispatch_inner` is now two functions instead of one; the `.text` was not compared.

## Deviations from the design discussion

- **Overlap is refused at arm time (a boot panic), not at compile time.** `Syscall` is not comparable in a `const fn`
  and the native number depends on the architecture, so a const assertion was not available.
- **The `own` tier sees the raw number, before the amd64 legacy fold**, so a legacy `mkdir` (83) is not
  `Call(Mkdirat)` there; the `kernel` tier sees the canonical call. aarch64 has no fold.
- **`own` context has `tid`, not `pid`:** the pid lookup takes process-table state; only `KernelCtx` has `pid`.
- **Not built yet:** a runtime `arm`/`disarm` handle for interest bits; a BKL-protected state cell for `kernel` hooks
  (use atomics or a `BorrowCell` static; the lock serialises them); typed `UserPtr<T>` (there are byte-level reads
  and `u64`/string helpers); a framework-provided blocking wait for `kernel` hooks (a hook that must park the
  caller has no sanctioned way to); `Return` from an `own` hook (the epilogue, signal delivery on the way out, runs
  under the lock, and whether a cheap lock-free "signal pending" test exists was not checked).
- **Untouched on purpose:** rump. Its interceptor (`akuma-kernel-glue::rump_proxy::intercept_box_syscall`) is a
  hand-written `kernel_pre` that blocks on a server and wants runtime-armed interest; B22 does not host it, and rump
  is deferred (`docs/reference/subsystems/rump-stack.md`). It is the API's acceptance test if it returns.

## Next

1. A/B the feature-on cost with a table that hooks nothing, then one that hooks `getpid`.
2. Decide whether the missing pieces above are wanted before anyone writes a second middleware.

## Background

`taoxichuan/docs/reference/syscalls.md` § Who answers, in order (the dispatch the table wraps);
`crates/akuma-bkl/src/policy.rs` (the per-syscall opt-out bitmap and the latch-at-entry rule);
`docs/archive/` W^X policy (`TAOXICHUAN_WX_POLICY.md`) for why there is no switch.

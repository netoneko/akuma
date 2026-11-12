# signal syscalls

rt_sigaction / rt_sigprocmask / sigaltstack / rt_sigsuspend / rt_sigtimedwait /
kill / tkill / tgkill / rt_sigreturn. Source: `src/syscall/signal.rs`; frame
build/unwind lives in `crates/akuma-exceptions/src/lib.rs` (`try_deliver_signal`,
`do_rt_sigreturn`). For `kill`'s process/tgid-wide routing see
[`proc.md`](proc.md) "kill / tkill"; for the blocking primitives behind
`rt_sigsuspend`/`rt_sigtimedwait` see [`../scheduler.md`](../scheduler.md)
"Blocking & wait/wake".

> **Stability: C (active risk).** Last touched 2026-11-11 (`kill`/`tkill`/`tgkill`
> are one copy in glue for every kernel and SIGKILL is a pend — see "`kill`, `tkill` and
> `tgkill`: one copy, and SIGKILL is a pend"); before that 2026-08-20 (`fork` gave the
> child an empty disposition table instead of the parent's — see "Disposition
> inheritance across fork and exec"); before that 2026-08-04 (fatal `SIG_DFL`
> signals arriving via the pending queue were dropped, `tkill` was being
> handed PIDs, and a `tkill`-pended signal could not interrupt a blocking
> syscall — see "Default action for pended signals" and "EINTR from a pended
> signal"); before that
> 2026-06-22, inside the Jun 2026 "memory + signal crisis" fire window
> (`docs/README.md`). The recurring
> lesson: **signal *disposition* is process-wide, signal *mask* and
> *sigaltstack* are per-thread** — `proc.signal_actions.actions` is a shared
> `Spinlock<[SignalAction; 64]>` (correct: POSIX handlers are shared across
> `CLONE_THREAD`), but the mask lives in per-thread storage
> (`threading::thread_signal_mask[_of]`) precisely because an earlier bug let
> one sibling's `rt_sigreturn` clobber another's blocked set
> (`archive/SIGNAL_DELIVERY_FORKTEST_EVIDENCE.md` §D).

## rt_sigaction

`sys_rt_sigaction` (`signal.rs:16`) copies a 32-byte `KernelSigaction
{ sa_handler, sa_flags, sa_restorer, sa_mask }` to/from user space and stores
it in `proc.signal_actions.actions[sig-1]` as an
`akuma_exec::process::SignalAction { handler, flags, mask, restorer }`.

- `sig == 9` (SIGKILL) or `19` (SIGSTOP) → `EINVAL`; their disposition can
  never change.
- `sigsetsize != 8` is tolerated (mask silently coerced to 0) rather than
  rejected — some libc callers pass unusual sizes.
- `handler` decodes `SIG_DFL`(0)/`SIG_IGN`(1)/anything else as a user PC
  (`SignalHandler::{Default,Ignore,UserFn}`).

**SA_RESTART** is implemented in `try_deliver_signal` (`exceptions.rs:994`),
not here: if the signal arrives while `ELR` is just past an `SVC` (EC 0x15)
*and* the syscall's return value was `-EINTR`/`-ERESTARTSYS`, `ELR` is backed
up 4 bytes so the handler returns straight into the syscall instruction. The
gate on the return value exists because an earlier version backed up `ELR`
unconditionally, re-executing an already-*successful* `FUTEX_WAKE` with
`x0=1` reinterpreted as `uaddr` → spurious `EINVAL`
(`archive/SIGNAL_DELIVERY.md` "The `SA_RESTART` / ELR backup bug").

`sa_mask` and `SA_NODEFER` are applied at delivery time, not registration:
`try_deliver_signal` ORs `action.mask` and (unless `SA_NODEFER`) the delivered
signal itself into the **per-thread** mask before jumping to the handler.
SIGKILL/SIGSTOP bits are always stripped from that OR.

## Disposition inheritance across fork and exec

| | disposition table | POSIX |
|---|---|---|
| `fork` / `vfork` | private **copy** of the parent's (`SharedSignalTable::clone_for_fork`) | inherited |
| `clone` with `CLONE_SIGHAND` (`clone_thread`) | the parent's table itself, shared by `Arc` | shared |
| `execve` | caught (`UserFn`) → `Default`; `Ignore` preserved; done in place on the same table (`Process::load_image`) | same |

`fork` handed the child a **fresh, all-`Default`** table until 2026-08-20,
which silently un-installed every handler the parent had registered. Worth
knowing about because of how narrow the observable blast radius is: `fork`
immediately followed by `exec` is unaffected — `exec` resets caught handlers
anyway — so only a process that forks and *stays in the same image* could ever
see it. That is precisely the master/worker daemon shape.

The failure it produced was two-headed, and neither head looked like a signal
bug:

1. The child could not be interrupted out of a blocking syscall.
   `current_thread_has_pending_interrupt` (below) only reports an interrupt for
   a `UserFn` handler, correctly — so with the disposition reset to `Default`
   the predicate answered "nothing deliverable" on every pass and an idle
   nginx worker sat in `epoll_pwait` through repeated `SIGTERM`s, looking
   immune to `kill`.
2. When the syscall did return for some unrelated reason, the `Default` action
   **terminated** the process rather than running its handler — killing an
   in-flight request instead of shutting down gracefully.

Full account: [`../../../archive/FORK_LOSES_SIGNAL_HANDLERS.md`](../../../archive/FORK_LOSES_SIGNAL_HANDLERS.md).
Regressions: `fork_signal_inheritance_tests` in
`crates/akuma-exec/src/process/signal.rs`.

## rt_sigprocmask / rt_sigsuspend

`sys_rt_sigprocmask` (`signal.rs:94`) reads/writes the **calling thread's**
mask (`threading::thread_signal_mask`) — never a process-shared field, since
`CLONE_THREAD` siblings must be able to block different signals. `SIG_BLOCK` /
`SIG_UNBLOCK` / `SIG_SETMASK` all strip the SIGKILL/SIGSTOP bits
(`(1<<8)|(1<<18)`) from whatever the caller passes.

`sys_rt_sigsuspend` (`signal.rs:151`) installs a temporary mask, arms a
"restore-sigmask" (`threading::set_restore_sigmask`) so the frame that
delivers the waking signal saves the *pre-suspend* mask as `uc_sigmask` (so
`rt_sigreturn` restores it), then polls `pending_signals_raw` in a
10 ms-sliced `schedule_blocking` loop until something not blocked by the
suspend mask (or an unblockable signal) is pending. Always returns `EINTR`,
per POSIX — it never returns via the normal 0 path. `pause` (x86_64 34, no
asm-generic number) is the same loop with the current mask.

All three kernels serve both from glue. **Whoever delivers signals at the
syscall return owns the slot from there:** a handler frame records the saved
mask as `uc_sigmask`, and a return that enters no handler must put the saved
mask back itself. The x86 kernels take the slot once per return
(`amd64/src/signal.rs::deliver_pending`, Taoxichuan's
`child::deliver_at_return`) and do both; AArch64 takes it only while building a
frame, so a woken-then-discarded signal leaves the temporary mask installed
(open, [`archive/TAOXICHUAN_B9_MADVISE_PIDFD_SIGSUSPEND.md`](../../../archive/TAOXICHUAN_B9_MADVISE_PIDFD_SIGSUSPEND.md)).

`rt_sigpending` (`sys_rt_sigpending`) answers the calling thread's pending
signals that its mask blocks (Linux's `do_sigpending`; Akuma pends every signal
on a thread).

## Pending signals: kill / tkill / tgkill

`sys_kill`'s tgid-wide fan-out is covered in [`proc.md`](proc.md) "kill /
tkill"; this doc covers the per-thread mechanics `kill` and `tkill` both
bottom out in.

- **Storage:** one `PENDING_SIGNAL[tid]` `AtomicU32` per thread slot — not a
  queue. A second `pend_signal_for_thread` before the first is taken
  **overwrites** it; only the later signal survives
  (`archive/SIGNAL_DELIVERY.md` "Single pending-signal slot limitation").
  Acceptable for coalescable async signals (SIGURG); not for e.g.
  SIGTERM-then-SIGKILL sequencing.
- **`take_pending_signal(mask)`** treats `mask` as the set of **blocked**
  signals: `deliverable = pending & (!mask | force_bits)`, where
  `force_bits = (1<<8)|(1<<18)` (SIGKILL, SIGSTOP). Passing `0` means "block
  nothing, take anything pending"; passing `!0` means "take only
  SIGKILL/SIGSTOP" — inverted from what it looks like at a glance, and the
  cause of a whole cluster of test-only bugs (`archive/SIGNAL_HELL.md` §2).
- **`sys_tkill`** (glue, every kernel) resolves the *target* thread's process
  and reads its mask and the process-wide handler table. **A tid it cannot
  attribute (no process, or one already gone) is dropped**, never treated as
  `Default`. `SIGKILL` → pend + `interrupt_thread`. Blocked → pend, no
  interrupt. `Ignore` → drop; `UserFn` → `pend_signal_for_thread` (delivered at
  the target's next return to user mode); `Default` with a fatal-by-default
  signal → pend + `interrupt_thread`, other defaults → drop. **No fatal default
  acts inside `tkill`**, the caller's own group included: the target dies at its
  own return, through that kernel's delivery (AArch64
  `apply_default_signal_action`, amd64 `deliver_pending`).
  **The decision is `akuma_syscalls_signal::tkill_action`** (host-tested, with an
  exhaustive sweep and a mask fuzz); `sys_tkill` only looks up the target and
  performs the returned `Drop`/`Pend`/`PendInterrupt`. Change the
  rule there, never inline. A kernel thread (no process) is a dropped target, so
  boot tests cannot reach the pend branch through `spawn_fn`; that branch is
  covered on the host (`archive/SIGNAL_FATAL_DEFAULT_LINUX.md`, correction).
- **`signal_is_fatal_default`** (forwarding to `akuma_syscalls_signal::fatal_by_default`) is Linux's disposition table: everything in
  `1..=64` except ignore-by-default (17, 23, 28) and stop/continue (18-22, no
  job control). The two `tkill` rules above are what make that safe under the
  `SIGUSR1` storm of a `rustc` build (`archive/SIGNAL_FATAL_DEFAULT_LINUX.md`).
- **The interrupt bit answers only calls that can block.** While a thread's
  interrupt bit is set, glue's dispatch prologue answers `EINTR` without running
  the call, **except** `rt_sigprocmask`, `rt_sigaction`, `rt_sigpending`,
  `rt_sigreturn`, `sigaltstack` and the signal senders `kill`, `tkill`,
  `tgkill`, Akuma's 302 and `pidfd_send_signal`, which run and leave the bit
  alone. The first five are how a program makes a pending signal deliverable;
  the senders never block, and Linux never fails any of them with `EINTR`. A
  `kill` answered `EINTR` here was silently not sent
  (`archive/SIGNAL_FATAL_DEFAULT_LINUX.md`,
  `archive/AKUMA_MULTIARCH_ATTEMPT_0.md` "Slice 3: signals").
- **`sys_tgkill`** adds one check over `tkill`: if the target tid resolves to
  a live process whose `tgid` doesn't match the caller's argument, `ESRCH`
  (prevents mis-delivery to a tid recycled into a different process); falls
  through to `tkill`'s own handling otherwise.
- `send_sigpipe()` (pipe writes) is just `sys_tkill(current_tid, 13)`.

### `kill`, `tkill` and `tgkill`: one copy, and SIGKILL is a pend

Every kernel runs glue's `signal::{sys_kill, sys_tkill, sys_tgkill}` (x86_64
62/200/234 reach them through `to_glue`; AArch64 129/130/131 and Akuma's 302
directly; Taoxichuan's arms call them by name). **No `kill(2)` hard-terminates a
thread from another core.** `sys_kill` decodes the `pid_t` (`> 0`, `0` own
group, `-1` everyone but init and the caller, `< -1` a group — the decode is
`akuma_exec::process::kill_target`, host-tested) and calls
`pend_signal_to_group` for every signal, 9 included: pend on each thread of the
group, raise the interrupt bit so a parked thread's wait returns `EINTR`, and
let the dying thread take the default action at its own return to user mode —
AArch64's syscall return or hooked IRQ return, amd64's `deliver_pending` /
`deliver_pending_on_tick`. `pidfd_send_signal` pends the same way.

`deliver_signal`'s SIGKILL arm (`kill_thread_group` + `kill_process_with_signal`,
marking every sibling `TERMINATED` from the caller's core after a 2 s grace) is
reached only by `kill_foreground_group` (console Ctrl-C) now. amd64 cannot
survive it at all: it has no consumer for the deferred kill that path posts
([`../../../archive/AKUMA_AMD64_SIGKILL_NATIVE_PATH.md`](../../../archive/AKUMA_AMD64_SIGKILL_NATIVE_PATH.md)).

**One teardown per thread group.** A group-wide pend makes every thread take
the fatal default at once, and each one calls `sys_exit_group`. The first
claims the group's exit (`SharedSignalTable::claim_group_exit`, Linux's
`SIGNAL_GROUP_EXIT`) and runs `kill_thread_group`; every later one marks its own
slot `TERMINATED` and parks, as a posted deferred kill does at the EL0 boundary.
Two teardowns in one group grace-wait on each other for the whole 2 s.

**A group's exit state is the group's, on both kernels.** The flag and the status live on the
group's `SharedSignalTable` (shared by `clone_thread`, fresh for `fork` and spawn, Linux's
`signal_struct`): `begin_group_exit`/`group_exiting` (siblings leave at their next syscall or
tick), `claim_group_exit` (exactly one teardown), and `GroupExit::{Code, Signal}` for a group
whose end a sibling decided (a sibling's `exit_group`, or its default-action signal death, which
no handler can catch). `execve` ends the exit it began to stop its siblings
(`end_group_exit_for_exec`). Never index group state by a process or task slot: slots are
recycled and the state outlives its process
([`../../../archive/AKUMA_AMD64_STALE_GROUP_EXIT_STATUS_241.md`](../../../archive/AKUMA_AMD64_STALE_GROUP_EXIT_STATUS_241.md)).

**A group's threads are the process table's rows with its `tgid`.** Every `CLONE_THREAD`
thread is a registered `Process` (own pid, the leader's `tgid`), so "which threads are still
running" is `akuma_exec::process::for_each_live_group_thread` / `live_group_threads` (members
other than the leader whose `THREAD_PID_MAP` slot is live), and "is this task a thread" is
`pid != tgid`. A leader's exit marks the group exiting **first** and counts after, and
`clone_thread` refuses a new thread once the group is exiting (`GROUP_EXITING`, `EAGAIN`), so no
thread can join after the leader's drain has counted to zero and its address space is freed. A
thread's `clear_child_tid` is its `Process`'s (set by `CLONE_CHILD_CLEARTID`, moved by
`set_tid_address`). Never keep a second thread list beside the table: amd64's slot-keyed one
outlived its threads and made the next process drain threads it never had
([`../../../../proposals/AKUMA_ONE_EXIT.md`](../../../../proposals/AKUMA_ONE_EXIT.md) step 3).

**Every blocking wait must leave on a deferred kill request**, not only on a pending signal:
a group exit asks a sibling to leave with `request_thread_kill` (`PENDING_KILL`), which pends
nothing. The common park loops read it through `should_interrupt_blocking_syscall`; a loop of
its own (`suspend_until_signal`, behind `pause`/`rt_sigsuspend`) must test `has_pending_kill`
itself, or a thread parked there outlives its group's drain
([`../../../archive/AKUMA_PAUSE_IGNORES_GROUP_EXIT.md`](../../../archive/AKUMA_PAUSE_IGNORES_GROUP_EXIT.md)).

Gates: `userspace/forktest/c_stress/killtree.c` (on both arches, through
`ssh`) and the amd64 boot check `usermode::kill_test`
(`userspace/amd64/killprobe`).

**`tid` here is a kernel thread slot, never a PID.** `pend_signal_for_thread`
and `thread_signal_mask_of` index the per-thread arrays directly with whatever
`tkill` was handed, so a caller that cached a PID signals an unrelated thread —
possibly in another process. See [`proc.md`](proc.md) "TID vs PID" for the
three syscalls that must publish the same value.

### Default action for pended signals

`try_deliver_signal` only ever installs a **user** handler; it returns `false`
for `SIG_DFL`/`SIG_IGN`. The pended-signal delivery sites in
`rust_sync_el0_handler` (normal syscall return, after `rt_sigreturn`, and the
JIT/IC-flush replay path) therefore call `apply_default_signal_action`
(`exceptions.rs`) on `false`: it re-reads the disposition and, for `SIG_DFL`
plus `signal_is_fatal_default`, calls `sys_exit_group_pub(-sig)`.

Re-reading the action is what separates "no handler" from the two re-pend
cases (`SA_ONSTACK` with no altstack yet, re-entrant on the altstack), which
are unreachable without a `UserFn` handler and so can never be mistaken for
"kill me".

Without this step a fatal `SIG_DFL` signal that arrived through the *pending*
queue was silently discarded — the queue is the only path for a signal pended
while blocked, so `abort()` (musl blocks SIGABRT, `tkill`s itself, then
unblocks) and `kill(pid, SIGTERM)` on a default-disposition process both did
nothing. `abort()` then fell through to musl's `a_crash()` (`strb wzr, [x0]`),
reporting **SIGSEGV at FAR=0** for what was really an undelivered SIGABRT.
Repro: `userspace/forktest/c_stress/abortsig.c`
(`archive/SELFHOST_DEVBOX_SMOLTCP.md` "SIGABRT delivery").

### EINTR from a pended signal (`pthread_kill`)

A pended signal wakes its target's waker, but waking is not interrupting. Every
blocking loop re-tests its own predicate on wake, so until 2026-08-04 they all
asked only `is_current_interrupted()` — a `ProcessChannel` flag set solely by
Ctrl-C and `sys_kill`. A `tkill`-pended signal therefore woke the thread, failed
that test, and the loop blocked again: **`pthread_kill` could never interrupt a
blocking syscall**, and the handler never ran either, since delivery happens at
syscall *return* and the syscall never returned.

Blocking loops now call **`should_interrupt_blocking_syscall()`**
(`process/children.rs`), which is `is_current_interrupted()` OR'd with the new
per-thread `current_thread_has_pending_interrupt()`. The latter reports true only
for a signal that is pending, **not blocked**, carries a `UserFn` handler, and
whose action **lacks `SA_RESTART`** — matching when Linux reports `EINTR` rather
than restarting. It early-outs on one atomic load when nothing is pending, so the
hot path is unchanged.

`SA_RESTART` needs no restart machinery here: these are all "retry until the
predicate holds" loops, so declining to report the interrupt *is* the restart.
That is what keeps Go working — its SIGURG preemption handler is installed with
`sa_flags=0x18000004` (`SA_ONSTACK|SA_RESTART|SA_SIGINFO`), so it never
interrupts a blocking syscall.

The `wait4`/`waitid` loops additionally moved their check to *before*
`schedule_blocking`, so a woken waiter re-tests `has_exited()` first. SIGCHLD
pends exactly when a child exits, so the two race by design and Linux hands back
the child; checking after the block would have turned the common successful wait
into a spurious `EINTR`.

Not covered: a signal that *does* set `SA_RESTART` still has its handler deferred
until the blocking syscall finishes, where Linux would run it immediately and then
restart. Nothing in the tree depends on the stricter timing, and the strict form
means re-entering blocking syscalls from scratch — which silently extends
`nanosleep`/`ppoll` deadlines (the reason Linux carries a `restart_block`).

Motivating case: jobserver-rs's `Helper::join` installs SIGUSR1 with
`SA_SIGINFO` and *no* `SA_RESTART`, then `pthread_kill`s its helper thread up to
100 times to break it out of a blocking pipe `read`. All 100 were burned and the
thread leaked — once per rustc that reaches codegen, quadrupled at `-j4`.
Kill switch: `config::PTHREAD_KILL_EINTR_ENABLED`.
Repro: `userspace/forktest/c_stress/pthread_kill_eintr.c` (A/B'd 2026-08-04 — flag off:
`read()` never returns, handler runs 0 times; flag on: `-1 EINTR` after 1 handler
run, with the `SA_RESTART` control unaffected in both).

## The `rt_sigframe` itself

The frame is a `#[repr(C)]` type,
`akuma_exec::threading::sigframe::RtSigFrame` — 1120 bytes: `siginfo_t` (128) +
`ucontext_t` header (176) + `sigcontext` (280) + an FPSIMD extension record (528) +
an `_aarch64_ctx` null terminator (8). **Read the layout there, not from offsets in
`exceptions.rs`**: the offsets are derived from the struct with `offset_of!` and
pinned by compile-time assertions, and both the builder and `rt_sigreturn` reach
user memory through a single validated copy each.

Two deliberate divergences from Linux, documented in that module and unchanged:
the FPSIMD record sits at frame+584 rather than +592 (Linux's `sigcontext` pads to
a 16-byte-aligned `__reserved`), and `__reserved` is 536 bytes rather than 4096.

## rt_sigreturn (frame unwind)

`sys_rt_sigreturn` (NR 139) is handled directly in `rust_sync_el0_handler`
(`exceptions.rs:2307`), not dispatched through `handle_syscall`.
`do_rt_sigreturn` restores the full GPR set (including `x8` — needed so the
next `SVC` sees the right syscall number, not the signal handler's), `sp_el0`,
`elr_el1`, and a sanitized `spsr_el1` (forces EL0t if the handler corrupted
the mode bits) from the `rt_sigframe` at `sp_el0`, restores the per-thread
mask from `uc_sigmask`, then restores FPSIMD (Q0–Q31, fpsr, fpcr).

The frame is built by `try_deliver_signal` (`exceptions.rs:964`): it writes
`siginfo_t` + `ucontext_t` (`uc_stack`, `uc_sigmask`) + `mcontext_t` (GPRs,
sp/pc/pstate) + an FPSIMD extension record onto the user stack (on the
sigaltstack if `SA_ONSTACK` and one is configured), demand-paging and
CoW-resolving the frame's pages first, then redirects `elr_el1` to the
handler with `x30` set to the restorer — the registered `sa_restorer`, or a
lazily-mapped kernel trampoline at a fixed VA (`SIGRETURN_TRAMPOLINE_ADDR =
0x2000`) for runtimes (Go) that rely on the vDSO instead.

Linux delivers pending signals on **every** return to user mode, including
after `rt_sigreturn` — so the NR 139 handler re-checks
`take_pending_signal` immediately after restoring the frame, before returning
`saved_x0` to userspace. Skipping this (the original bug) let a signal that
arrived mid-handler leak into the *next* syscall's arguments instead of being
delivered promptly (`archive/SIGNAL_DELIVERY.md` "The bug: missing signal
delivery after rt_sigreturn").

Re-entrant delivery (handler faults, or a non-fault signal arrives while
already on the sigaltstack) is detected by checking whether `sp_el0` falls
inside the configured altstack range: a fault re-pends fatal instead of
looping, a non-fault signal is re-pended for after the current handler
returns.

## sigaltstack / rt_sigtimedwait

`sys_sigaltstack` stores/reads a per-thread `(sp, size, flags)` triple
(`threading::{get,set}_sigaltstack`); `SS_DISABLE` clears it; a non-disabling
`ss_size < 2048` (`MINSIGSTKSZ`) is rejected with `ENOMEM`.

`sys_rt_sigtimedwait` synchronously polls `take_pending_signal(!wait_mask)`
(inverting `wait_mask` because the primitive wants *blocked*, not
*wait-for*, bits) in the same 10 ms-capped `schedule_blocking` loop pattern as
`rt_sigsuspend`, returning the signal number, `EAGAIN` on timeout, or `EINTR`
if externally interrupted. `siginfo_t` fill-in is minimal (signo only).

## Background

- `archive/SIGNAL_DELIVERY.md` — the rt_sigreturn missed-delivery bug, the
  SA_RESTART/ELR backup bug, mask bit-numbering, the single-pending-slot
  limitation.
- `archive/SIGNAL_DELIVERY_FORKTEST_EVIDENCE.md` — code-audit findings behind
  the per-thread mask/sigaltstack split (§D) and several frame-safety fixes
  (FPSIMD alignment, `do_rt_sigreturn` bounds validation, `sa_mask` applied
  at delivery, SPSR sanitization).
- `archive/SIGNAL_HELL.md` — the pending-signal-bitmask test cluster (mask
  semantics were correct; the tests were inverted), the thread-group
  kill/exit_group cluster, and the crush goroutine-coordination stall
  (root cause: IRQ-disabled UART writes, not the signal primitives).
- `archive/SIGNAL_FATAL_DEFAULT_LINUX.md` — why `signal_is_fatal_default`
  once excluded SIGUSR1/2 and the real-time range (`archive/AKUMA_SELF_HOSTING.md`
  §7k.5), and the `tkill` attribution fix that let it follow Linux.

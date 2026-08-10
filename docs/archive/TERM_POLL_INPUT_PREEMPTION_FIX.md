# The terminal-state preemption wedge: `poll_input_event` spinning with preemption off

**Date:** 2026-08-11 (analysis), refined 2026-08-11 (follow-up, §9-§10).
**Issue first observed:** 2026-08-10.
**Kernel at capture:** devbox-smoltcp, `smp-shared` default feature set, `SMP=2`,
no gdbstub. **Status:** MECHANISM ROOT-CAUSED (§9) — the code pattern that makes
the wedge possible *and unbounded, regardless of who the holder is*, is now
proven from the BKL/scheduler source, not just hypothesized. A fix is designed
(§10) but **not yet implemented**. The specific holder that produced the
94-second spin in the captured incident is still not identified — §9's finding
is that identifying it was never necessary: the defect is in how the waiter
waits, not in who it waits for.

**One line:** the kernel's blocking stdin-read loop
(`sys_poll_input_event` in `src/syscall/term.rs:405-437`, mirrored by
`sys_read`'s `Stdin` arm in `src/syscall/fs.rs:384-453`) takes the per-process
`term_state_lock` and its nested `input_waker` spinlock with **preemption
disabled but IRQs enabled**, and the post-wake re-acquire at `term.rs:432` can
sit in that state long enough under SMP contention for the preemption watchdog
to declare the whole VM stuck.

Current-state writeup (the doc to read first):
[`../reference/subsystems/syscalls/term.md`](../reference/subsystems/syscalls/term.md)
→ "Blocking stdin read — `poll_input_event`". Diagram + hazard callout live
there; this doc is the historical investigation.

Observed-from report:
[`DEVBOX_ISSUES.md`](DEVBOX_ISSUES.md) Issue 2 — this doc is the deep-dive on
that one issue, written so the playbook entry can stay short.

---

## 1. The shape of the loop

`sys_poll_input_event` (`src/syscall/term.rs:376-448`) handles three modes
based on `timeout_us`:

- `0` — non-blocking drain, return immediately.
- `u64::MAX` — block forever.
- anything else — block with a deadline.

For both blocking modes the body is the same poll-wait loop (`term.rs:405-437`):

```rust
loop {
    {                                                   // (A) register waker
        akuma_exec::threading::disable_preemption();
        let term_state = term_state_lock.lock();
        let thread_id = akuma_exec::threading::current_thread_id();
        term_state.set_input_waker(
            akuma_exec::threading::get_waker_for_thread(thread_id));
        akuma_exec::threading::enable_preemption();
    }

    let n = proc_channel.read_stdin(&mut kernel_buf);   // (B) non-blocking drain
    if n > 0 { bytes_read = n; break; }

    if akuma_exec::process::should_interrupt_blocking_syscall() {
        return i64::from(-libc_errno::EINTR) as u64;
    }

    if crate::timer::uptime_us() >= deadline {
        bytes_read = 0; break;
    }

    akuma_exec::threading::schedule_blocking(deadline); // (C) park

    {                                                   // (D) clear stale waker
        akuma_exec::threading::disable_preemption();
        let term_state = term_state_lock.lock();        // ← term.rs:433 (col reported as :432)
        term_state.input_waker.lock().take();
        akuma_exec::threading::enable_preemption();
    }
}
```

The sibling `sys_read` Stdin arm (`src/syscall/fs.rs:384-453`) is structurally
identical — same (A)(B)(C)(D) blocks, with the same two
`disable_preemption()` → `term_state_lock.lock()` critical sections — plus an
extra re-check-and-clear step (`fs.rs:438-443`) that closes a lost-wakeup race
against `close_process_stdin`. **Every hazard below applies to both call
sites.** That mirror is why
[`locking.md`](../reference/subsystems/locking.md) gate 2 lists this surface
as the thing blocking the BKL-free conversion of `read` itself.

## 2. What the writers look like

Two producers feed stdin and wake the reader: `write_to_process_stdin`
(`crates/akuma-exec/src/process/mod.rs:309-328`) and `close_process_stdin`
(`mod.rs:336-353`). Both run **BKL-held** (callers are SSH-channel dispatch,
which never drops the BKL), and both take the same lock with the same
discipline:

```rust
crate::threading::disable_preemption();
if let Some(waker) = proc.terminal_state.lock().input_waker.lock().take() {
    waker.wake();
}
crate::threading::enable_preemption();
```

Note the nested lock: `terminal_state.lock()` (outer) then `.input_waker.lock()`
(inner). The reader takes them in the same order. There is no second nested
acquisition in the reader's clear path — it relies on the outer
`term_state_lock` already being held.

A third producer is `TerminalState::push_input`
(`crates/akuma-terminal/src/lib.rs:132-142`), used by the kernel built-in
console / SSH-input path. It locks `input_buffer` then `input_waker` in
sequence (not nested under `term_state_lock`), and is reachable without
disabling preemption — a discipline mismatch of the same class as the
`ProcessChannel` mixed-discipline bug in `locking.md` ("Every caller of a lock
must agree on IRQ discipline"), but on a different lock.

## 3. Why `disable_preemption()` is the wrong shape here

`threading::disable_preemption()` (`crates/akuma-exec/src/threading/mod.rs:1791`)
increments a per-thread counter `PREEMPTION_DISABLED[tid]`. While it is
non-zero, the timer IRQ's scheduler entry (`schedule_indices`) returns early
instead of switching off this thread. Two properties matter:

1. **It is per-thread, not per-core.** It does not stop IRQs from firing; it
   only stops the scheduler from acting on them for *this thread*. IRQs are
   still delivered and their handlers still run.
2. **It does not coordinate with other cores at all.** Two cores can both be
   inside `disable_preemption()` simultaneously, and both can spin on the same
   `Spinlock`.

Contrast with `crate::irq::with_irqs_disabled`, which masks this core's IRQs
entirely (the legacy single-core mutual-exclusion primitive). Under
`smp-shared`, neither primitive alone is sufficient: `with_irqs_disabled`
gives no cross-core exclusion, and `disable_preemption` gives neither cross-
core exclusion nor IRQ masking.

`locking.md` rule "Mask IRQs/preemption per *attempt*, never across an
unbounded wait" and gate 2 ("Every inner lock the excursion takes must mask
local IRQs") encode the consequence: a lock reachable from a BKL-free window
that is taken with only `disable_preemption()` is a live AB-BA vector with
nested IRQ handlers' unconditional `enter_kernel()`. **The
`term_state_lock`/`input_waker` sites are exactly that vector**, and they are
the reason `read`'s Stdin arm has not been moved off the BKL.

The Issue 2 wedge is a *different* failure mode of the same pattern, observed
before any BKL-free conversion of these paths.

## 4. The wedge in the captured incident

From [`DEVBOX_ISSUES.md`](DEVBOX_ISSUES.md) Issue 2, on a devbox-smoltcp VM
running `meow` interactively (TUI idle poll), `SMP=2`:

```
[BKL] stuck: owner=2 waiter=1 tag=511 (aff0+1)
[WATCHDOG] Preemption disabled for 1113ms at step 6 tid=11
[WATCHDOG] disabled at src/syscall/term.rs:432
...
[WATCHDOG] Thread 11 preemption disabled 94132ms (critical)
```

- `tid=11` = `pid=15` = `/bin/meow`, `last_core=0`. Confirmed by
  `[THR-DUMP]`/`[PSTATS]`.
- `owner=2 waiter=1` are core IDs (`aff0 + 1`): **core 1 holds the BKL,
  core 0 is spinning for it** — confirmed at `crates/akuma-exec/src/sync.rs:819`
  (`held_by == core_id + 1`) and `log_kernel_lock_stuck` (`sync.rs:833-845`).
- `tag=511` is profiler-off noise (`HOLD_TAG_UNKNOWN`), not a finding.
- `disabled at src/syscall/term.rs:432` is the `disable_preemption()` site
  immediately before block (D) above — the post-wake clear-waker section.

Two `[THR-DUMP]`/`[PSTATS]` snapshots 30 s apart showed byte-for-byte identical
syscall counts for every thread (no forward progress). qemu pegged at ~199%,
SSH went unresponsive, no panic.

### 4.1 What the watchdog tells us, precisely

The preemption watchdog only reports that **the currently-running thread on
core 0 has had preemption disabled for ≥N ms**, and which source location
raised the counter. It does **not** say:

- whether that thread is spinning in `term_state_lock.lock()` or doing
  something else inside the critical section, or
- who, if anyone, holds `term_state_lock`, or
- whether the `[BKL] stuck owner=2` line describes the same logical moment as
  the preemption-disabled spin or an adjacent one.

So the literal evidence supports only: *meow's tid=11, on core 0, with
preemption disabled at `term.rs:432`, made no forward progress for 94 seconds,
and the BKL was simultaneously reported stuck with core 1 as owner and core 0
as waiter.* The rest is mechanism.

### 4.2 The simplest mechanism that fits

tid=11 reaches `term.rs:432` only after `schedule_blocking(deadline)` at line
429 returned — i.e. after being woken (its `WOKEN_STATES` flag was set by a
producer) or after its deadline expired. For the post-wake cleanup block to
stall on `term_state_lock.lock()`, **some other thread must be holding
`term_state_lock` at that instant**. The only candidates are:

- a writer (`write_to_process_stdin` / `close_process_stdin`) inside its own
  `disable_preemption()` critical section, or
- another reader's (A) or (D) block on the same `TerminalState` (possible if
  the `Arc` is shared, e.g. across a `pty`-spawned session that *didn't* get a
  fresh `TerminalState` — but see [`syscalls/term.md`](../reference/subsystems/syscalls/term.md)
  footnote: `pty` spawns deliberately *do* mint a fresh Arc, so intra-meow
  aliasing is unlikely), or
- an ioctl path (`TIOCSWINSZ` / `TIOCGPGRP` / `TIOCSPGRP` / `TCSETS`) holding
  `term_state_lock` (`term.rs:116-119`, `252`, `272`, `210`).

Whichever it is, the holder is almost certainly **BKL-held** (every producer
and ioctl runs BKL-held at HEAD). Under `smp-shared` the holder therefore
needs to either keep running on its current core or win the BKL again after a
preemption to finish its critical section. The waiter (tid=11) is sitting with
preemption disabled, so:

- **Core 0 cannot schedule anyone else** while tid=11 spins. Any thread that
  needs core 0 to make progress — including any thread that the holder might
  be waiting on — is starved.
- The preemption watchdog counter (`PREEMPTION_DISABLED_SINCE[11]`) climbs
  monotonically until the lock lands. There is no timeout and no abort.

That is sufficient to produce a permanent stall **if the holder cannot reach
its `enable_preemption()` for an unrelated reason** (e.g. it is itself waiting
on the BKL, which is held by a third thread that itself wants something that
core 0 is no longer servicing). Once the cycle closes, no participant makes
progress, and because preemption is off on core 0 the heartbeat goes silent.

### 4.3 What is *not* proven

- **The identity of the holder.** The Issue 2 writeup's circumstantial
  attribution to `sshd`'s old single-threaded poll loop is explicitly
  retracted there (sshd went process-per-session the same day); it cannot be
  used to explain a fresh occurrence. A repro on a current image would
  attribute differently.
- **Whether the `[BKL] stuck` and the preemption spin are the same logical
  moment or two adjacent ones.** The watchdog cadence (~30 s heartbeat) is
  too coarse to capture the moment of entry into either critical section.
- **Whether the holder is on core 1 specifically.** `owner=2` proves only
  that *some* thread on core 1 held the BKL at the watchdog's sample instant;
  it does not prove that thread was the `term_state_lock` holder.

## 5. Hypotheses, ranked by current evidence

| # | Hypothesis | Evidence for | Evidence against / gap |
|---|---|---|---|
| H1 | A BKL-held writer (`write_to_process_stdin`/`close_process_stdin` from an sshd session) holds `term_state_lock` across an `enable_preemption()` it cannot reach, because it is itself blocked on something the spinning core 0 should be servicing. | Matches the wedge shape; writers are the dominant producers of input-waker wakes; the cycle is structurally possible. | No witness capture of the holder's identity. The cycle has to be closed by *some* third dependency; not yet enumerated. |
| H2 | An ioctl on `term_state_lock` (`TIOCGPGRP`/`TIOCSPGRP`/`TCSETS`/`TIOCSWINSZ`) from another session holds the lock long enough to stall the post-wake clear. | Same shape; ioctls run BKL-held and take the same lock; `TIOCSWINSZ` on a `ChildStdout(child_pid)` fd additionally takes a `lookup_process_shared` inside the critical section. | Not specifically implicated. Most ioctls hold the lock for microseconds. |
| H3 | The same AB-BA-with-nested-IRQ hazard `locking.md` gate 2 describes: an IRQ fires while tid=11 is in block (D) with preemption disabled (but IRQs enabled), the IRQ handler calls `enter_kernel()` for the BKL, and the BKL owner on core 1 is itself waiting (elsewhere) on `term_state_lock`. | Structurally available — IRQs are enabled inside `disable_preemption()`. Gate 2 was written from a real hazard of exactly this shape on neighbouring locks. | Would not, by itself, explain a 94-second freeze; an AB-BA deadlock either panics or recovers. Would need the IRQ's `enter_kernel` to hard-spin, which it does. Plausible contributor, not sole cause. |
| H4 | Preemption disabled inside block (D) prevents the timer tick from running the scheduler on core 0, so a wake that should land on a *different* core-0 thread (e.g. the BKL holder that needs to migrate to core 0 to release `term_state_lock`) never lands. | True by construction — that is exactly what `disable_preemption` does. | Describes a contributing condition, not the trigger; needs H1 or H2 to supply the contending holder in the first place. |
| H5 | Reader-reader aliasing — two readers sharing one `TerminalState` race for the post-wake clear. | None: `pty` spawn deliberately mints a fresh Arc. | Inconsistent with the spawn model documented in `syscalls/term.md`. |

H1+H4 is the most defensible composite picture; H3 is a co-resident hazard
that the same fix would close.

## 6. The fix space (not prescriptive, not yet implemented)

Listed in increasing order of invasiveness. Each has tradeoffs that need a
decision before code lands.

1. **Take the lock with IRQs masked, not preemption disabled.** Replace
   `disable_preemption()` + `term_state_lock.lock()` with
   `irq::with_irqs_disabled(|| term_state_lock.lock())` (or the `PreemptGuard`
   shape used elsewhere in the network carve-outs, which masks both). Closes
   H3 (the AB-BA-with-nested-IRQ) by construction; extends the worst-case
   IRQ-disabled window per critical section. Matches what `locking.md` gate 2
   demands before `read`'s Stdin arm can be BKL-free. **Does not, by itself,
   fix H1+H4** — it makes the waiter's spin IRQ-masked instead of
   preempt-disabled, but the spin still happens if the holder can't release.
   It only fixes the wedge if the mechanism was H3.

2. **Make `input_waker` lock-free.** `input_waker` is a single
   `Option<Waker>` with no companion state — exactly the
   `UTC_OFFSET_US`-becomes-`AtomicU64` precedent cited in `locking.md`. A
   tagged atomic (`Waker` pointer + generation counter, take-and-clear by
   CAS) would remove one of the two locks from the path entirely. This is the
   phase-7g classification (`BKL_FINE_GRAINED_LOCKING_PLAN.md` §7.3a). Removes
   the inner nested-lock acquire from both (A) and (D), leaving only
   `term_state_lock` itself.

3. **Restructure the handshake.** Register the waker once *outside* the loop,
   clear it once after the loop exits. Tolerate stale wakers via the
   `schedule_blocking` sticky-wake (`WOKEN_STATES`, `threading/mod.rs:3532`) —
   a stale wake just re-enters the loop, drains nothing, and parks again.
   This turns (A) and (D) from per-iteration critical sections into
   once-per-syscall setup/teardown, dropping the spin frequency by orders of
   magnitude. The `sys_read` Stdin arm's extra `is_stdin_closed` re-check
   (`fs.rs:438-443`) was added for exactly the kind of lost-wakeup this
   restructure would have to keep covering.

4. **Bound the spin.** A `try_lock` with a bounded retry count, falling back
   to `schedule_blocking` if it can't acquire promptly. Closes H4 (no thread
   is ever stuck with preemption off for seconds). Adds complexity and a new
   parking path; doesn't address H3.

5. **Drop the BKL across `schedule_blocking` (already done for
   `blocking_relax`).** The M5d work (`smp-shared.md`) routes blocking waits
   through `blocking_relax()` with no lock held. *This loop does already
   release preemption inside `schedule_blocking`* (`threading/mod.rs:3543-3563`
   force-enables preemption to park, then restores it). So this is not the
   M5d bug; it's a different property of the same family.

Any fix should be evaluated against **both** reproducers:

- the original (idle TUI polling under load — produces the watchdog signature
  in the captured incident), and
- a synthetic contention harness (multiple readers + writers on the same
  `TerminalState` Arc, e.g. a `pty` session under an SSH session, ideally at
  `SMP≥2`) to expose the H3 AB-BA path on its own.

## 7. What to capture on the next repro

The Issue 2 capture has no gdbstub and no per-acquire trace. The next repro
should add, even temporarily:

- **GDB=1** boot, so a stuck window can be halted and `term_state_lock`'s
  owner inspected (the lock is a `spinning_top::RawSpinlock` with no owner
  field, but every core's current PC + the live `THREAD_STATES` snapshot is
  enough to attribute).
- **`[TERM-LOCK]` acquire/release trace** at the four sites in the loop and
  the two writer sites, with core id, tid, and a monotonic counter. Without
  this, the 30 s heartbeat is too coarse to see *entry*.
- **`bkl-profile` build** so `[BKL] stuck` is no longer `tag=511` noise —
  the holder's tag would name the syscall or subsystem, which is precisely
  what the current capture lacks.

A repro that captures the holder's identity would convert H1/H2 from
"plausible" to "proven" or rule them out.

## 8. Adjacent facts worth recording

- **`disable_preemption` is nestable** (`fetch_add`/`fetch_sub` on a counter,
  `threading/mod.rs:1793,1815`). The loop's (A) and (D) blocks each form a
  balanced pair, but a nest across `schedule_blocking` (e.g. an outer caller
  having preemption disabled) would silently extend the spin window.
  `schedule_blocking` itself detects and force-enables preemption to park
  (`mod.rs:3543-3563`), so a nest across (C) is recovered — but only at the
  park boundary, not at the surrounding (A)/(D) critical sections.
- **`schedule_blocking`'s sticky-wake handshake** (`WOKEN_STATES`,
  `publish_waiting_and_take_pending_wake`, `mod.rs:3531-3563`) is what makes
  the `Option 3` restructure viable: a wake that lands between (A) and (C)
  is recorded sticky and re-read on the way into the park, so a missed
  registration does not leak.
- **The `sys_read` Stdin arm and `sys_poll_input_event` are not the only
  readers of `input_waker`.** `TerminalState::push_input`
  (`akuma-terminal/src/lib.rs:138`) and the channel's own drain logic also
  take it. Any fix that changes the lock discipline must update all sites in
  lockstep — the same rule that bit `ProcessChannel`'s mixed
  IRQs-on/IRQs-off callers (see `locking.md` correctness-rule of the same
  name).

## 9. Refined root cause (2026-08-11 follow-up)

Section 5 ranked hypotheses without being able to prove any of them; the
holder's identity blocked closing the loop. Tracing the BKL model
(`crates/akuma-exec/src/bkl.rs`) and the scheduler's parking path
(`crates/akuma-exec/src/threading/mod.rs`) precisely shows that the holder's
identity was never load-bearing: the loop shape guarantees an *unbounded* wait
whenever `term_state_lock` happens to be contended by a holder that itself
needs to enter the kernel to finish, independent of which hypothesis (H1/H2)
supplies that holder.

### 9.1 The BKL is single, global, and cross-core-exclusive

`docs/reference/subsystems/locking.md` states the model as "held iff a core is
in EL1" — that undersells how strict it is. `KernelLock` (`crates/akuma-exec/src/sync.rs`)
is one process-wide ticket lock: **at most one core may be executing kernel
code at any instant**, full stop. Ownership is reconciled per-core only at an
`eret` boundary (`bkl::reconcile_for_spsr`, `bkl.rs:344-350`), which inspects
the *destination* SPSR of whatever the eret is about to resume: target EL0 →
release; target EL1 (and no dropped-window) → acquire/keep. Nothing about a
context switch *between two kernel-mode threads on the same core* touches this
at all — that's just a same-privilege-level resume, no eret, no reconcile call.

`sys_poll_input_event` and `sys_read`'s Stdin arm are not on `SYSCALL_BKL_OPTOUT`
(confirmed absent from the tranche list in `locking.md` — precisely *because*
gate 2 is open for this lock). So the whole syscall body, including every
iteration of the (A)(B)(C)(D) loop, runs BKL-held, with one exception: while
actually parked inside `schedule_blocking`.

### 9.2 `schedule_blocking` does not drop the BKL — but that's usually fine

`schedule_blocking`'s wait loop (`threading/mod.rs:3576-3605`) is a bare
`wfi`-spin on `THREAD_STATES`/`WOKEN_STATES`, with **no `bkl::leave_kernel()`
call** — unlike `idle_halt()` (`threading/mod.rs:2948-2990`), which explicitly
calls `bkl::leave_kernel()` before its own `wfi` (line 2968) precisely because
an idle core must not hold the BKL. `schedule_blocking` doesn't need to: it
marks the thread WAITING and requests an immediate voluntary reschedule
(`voluntary_schedule_flag` + `trigger_sgi(0)`, `threading/mod.rs:3572-3573`),
so in the common case the *core* moves on to a different READY thread almost
immediately — and that thread's own resume dictates the reconcile: if it was
suspended in EL0 (the common case — most ready threads are usermode-resident),
resuming it erets to EL0 and legitimately drops the BKL for as long as it runs
there. This is normal, high-frequency churn on any loaded core, and it's why
the doc's §6 point 5 already correctly concluded this is *not* the M5d bug
(`blocking_relax` covers a different, already-fixed parking path).

The parked thread only re-acquires the BKL when it is *itself* rescheduled
back in — its saved context targets EL1 (it never left the kernel), so that
resume's reconcile call acquires. If another core holds the BKL at that exact
moment, this is a legitimate, normal, momentary `[BKL] stuck` sample — exactly
the shape of the `owner=2 waiter=1` line in the capture, and it resolves
quickly under ordinary conditions.

### 9.3 The actual defect: `disable_preemption()` wraps the *wait*, not the *hold*

This is the part that turns ordinary contention into an unbounded wedge.
Blocks (A) and (D) (`term.rs:405-412`, `term.rs:431-436`; mirrored at
`fs.rs:424-430`, `fs.rs:447-452`) do:

```rust
akuma_exec::threading::disable_preemption();
let term_state = term_state_lock.lock();   // blocking spin-acquire
...
akuma_exec::threading::enable_preemption();
```

`disable_preemption()` is called *before* the blocking `.lock()`, not after
acquiring it. If `term_state_lock` is contended, the calling thread spins
inside `.lock()` with its own preemption already disabled for the whole spin —
not just for the microsecond-scale critical section that follows. Per
`disable_preemption`'s own contract (`threading/mod.rs:1782-1807`, §3 above),
this specifically
vetoes the *local, involuntary* scheduler switch-away of the current thread —
so a contended spin here doesn't just fail to make progress, it **monopolizes
the entire core**, with no timeout and no fallback, for as long as the lock
stays taken.

And because that same syscall is BKL-held per §9.1, monopolizing the core also
monopolizes the sole global BKL. If the actual `term_state_lock` holder —
under H1, a writer (`write_to_process_stdin`/`close_process_stdin`,
"BKL-held, never dropped"); under H2, an ioctl — is on a *different* core and
has not yet entered its own BKL-held excursion, it now needs the BKL to even
begin its critical section, and cannot get it, because the spinning core holds
it hostage. The cycle is now fully closed and self-contained, independent of
which specific hypothesis supplies the holder:

> The spinning thread can never acquire `term_state_lock` (the holder can't
> run to release it). The holder can never run (the spinner holds the BKL and
> cannot be preempted off its core to give it up). Neither side has a timeout.

This matches the capture precisely: no panic, no forward progress, `qemu`
pegged (one core hard-spinning), SSH unresponsive (every other core frozen on
the BKL), and a watchdog counter that only grows.

### 9.4 This is exactly the anti-pattern the project has already named and fixed elsewhere

`docs/reference/subsystems/locking.md:222-229` states the rule directly:

> **Mask IRQs/preemption per *attempt*, never across an unbounded wait.**
> `read_state`/`write_state`-style acquisition loops can spin for a long time
> ... Masking IRQs across the whole wait starves this core's timer for the
> entire contended window — and if the current holder is a thread *on this
> core*, nothing can ever run to release it. Take the preempt/IRQ guard
> immediately before the non-blocking `try_lock()`, keep it only on success,
> drop it before the backoff spin.

`crates/akuma-ext2/src/ext2.rs`'s `read_state`/`write_state`
(`ext2.rs:770-845`) already implement this correctly: a per-attempt
`state_hold_guard()` (`ext2.rs:591-596`, a `PreemptGuard` under `no-bkl-vfs`)
taken immediately before a non-blocking `try_read()`/`try_write()`, dropped
before the backoff spin if the attempt fails. `term_state_lock`'s (A)/(D)
blocks are the same shape of lock (a spinlock with a possibly-long,
holder-dependent wait) implemented the *wrong* way — guard-then-blocking-lock
instead of guard-per-attempt. §10 below adopts the `read_state`/`write_state`
pattern directly.

## 10. Fix plan (decided 2026-08-11, not yet implemented)

Two independent, complementary changes:

**1. Bound the spin — the actual correctness fix.** Replace
`disable_preemption(); term_state_lock.lock(); ...; enable_preemption();` with
a per-attempt `try_lock()` retry loop, mirroring `Ext2Filesystem::read_state`/
`write_state` (§9.4): take the preemption guard immediately before a
non-blocking `try_lock()`, keep it only on success (for the brief hold), drop
it before backing off and retrying on failure. This guarantees no thread, core,
or the BKL can ever be held hostage longer than one lock attempt, regardless of
how long the real holder takes to become schedulable. Applies to all 6
existing sites:

- `src/syscall/term.rs` — blocks (A) and (D) in `sys_poll_input_event`
  (`term.rs:405-412`, `term.rs:431-436`).
- `src/syscall/fs.rs` — the mirrored blocks in `sys_read`'s Stdin arm
  (`fs.rs:424-430`, `fs.rs:447-452`).
- `crates/akuma-exec/src/process/mod.rs` — `write_to_process_stdin`
  (`mod.rs:309-328`) and `close_process_stdin` (`mod.rs:336-353`), for
  discipline consistency with the readers even though they were not observed
  stuck (their critical sections are short and don't spin-wait on contention
  today, but they take the same lock and should not be the exception to the
  rule).

**2. Cut the frequency — doc's original fix option 3, `term.rs` only.**
Register the input waker once before the loop in `sys_poll_input_event` and
clear it once on every exit path (success, timeout, `EINTR`), instead of once
per iteration. `schedule_blocking`'s sticky-wake (`WOKEN_STATES`) already
tolerates a stale registered waker (a wake between register and park is
recorded and re-read on the way into the park), so this is safe per the
analysis already in §6/§8. Scoped to `term.rs` only: `sys_read`'s Stdin arm
(`fs.rs`) has multiple early-return exit points and a canonical-mode `continue`
path, so "register once / clear on every exit" has more places to get wrong
and risks reintroducing the exact lost-wakeup race its own re-check comment
(`fs.rs:432-437`) was written to close. Fix #1 alone already closes the hang in
`fs.rs`; applying #2 there too is optional future hardening, not required for
correctness.

**3. Fix the discipline mismatch in `TerminalState::push_input`/`read_input`.**
`crates/akuma-terminal/src/lib.rs:132-158` — `push_input` locks `input_buffer`
then `input_waker` **without disabling preemption or masking IRQs at all**,
unlike every other producer/consumer of `input_waker`. These two methods are
currently dead code (no caller anywhere in `src/`, `crates/`, or `userspace/` —
confirmed by a full-tree grep), so they cannot contribute to the live wedge.
Fixing them anyway, to the same per-attempt-guarded discipline as #1, removes a
latent trap for whoever wires them up later (matching `locking.md`'s
"every caller of a lock must agree on IRQ discipline" rule, cited in §2 above).

**4. Kernel self-tests** (`src/process_tests.rs`, boot-suite): a test that
constructs a real `Arc<Spinlock<TerminalState>>`, holds it from one thread for
an artificially long window, and asserts a second thread doing the fixed
per-attempt acquire makes bounded-time forward progress instead of spinning —
plus a check that `PREEMPTION_DISABLED_SINCE` for the acquiring thread never
exceeds a small bound during the contended path. This directly regressions-
tests §9.3's mechanism, independent of reproducing the full syscall/BKL stack.

**5. `userspace/termtest` stress flag + README.** `fork()`
(`crates/akuma-exec/src/process/mod.rs:2211-2228`) clones the parent's
`terminal_state`, `channel`, and `stdin` as shared `Arc`s into the child —
unlike `spawn_pty`, which deliberately mints a fresh `TerminalState` per spawn
(the reason §5's H5 ruled out reader-reader aliasing for pty sessions
specifically). A plain `fork()`-based stress mode is therefore a self-contained
way to manufacture real multi-thread contention on one `TerminalState` without
needing a human typing or a second SSH session: fork N children off one
`termtest` process, each hammering blocking `poll_input_event` and/or
terminal ioctls (`TIOCGWINSZ`/`TCSETS`, which also take `term_state_lock`) in a
tight loop with heartbeat prints, run under `SMP>=2`. A wedge shows as a child
that stops printing its heartbeat. Plan: add a `--stress [N]` flag to the
existing `termtest` binary rather than a new binary, plus a
`userspace/termtest/README.md` documenting the program, how the stress mode
reproduces the wedge, how to run it, and a pointer back to this doc.

## 11. Implementation status (2026-08-11)

Fixes #1-#3 and #5 above are **implemented and verified on-device** (SMP=2,
`INSTANCE`-isolated boot: clean boot to `sshd`, `cargo clippy`/host tests
clean, `termtest --stress 4` and `--stress 8` both PASS with all forked
children completing and printing heartbeats throughout).

Fix #4's **kernel self-test is written but currently disabled** — it has its
own synchronization bug (a real, reproducible on-device hang, confirmed via
bisection to be in the test's holder/canary/main rendezvous, not in
`lock_bounded` itself: the *production* code paths that use `lock_bounded`
all verified clean in the same boot). The call site is commented out in
`src/process_tests.rs::run_all_tests` with a note; the test function itself
is `#[allow(dead_code)]`. Needs a fresh synchronization design before
re-enabling — see the comment at the call site for the observed symptom.

While chasing this, an early false lead is worth recording: a **different**
pre-existing test (`test_mixed_cooperative_preemptible`, `src/tests.rs:2466`,
which runs *before* anything in this fix is reachable) was initially
suspected of hanging too, based on truncated log snapshots taken while a
`[BKL] stuck` burst was mid-flight. Given more time to run, that test
actually passes (`Result: PASS`) and boot continues normally past it — the
transient `[BKL] stuck` bursts it (and the wider boot) print are the known
load-driven, self-healing kind (see `bkl_tag511_storm_is_load_driven` in
project memory), not a hang. Don't mistake a live, still-growing log for a
wedge — check the file is still growing before concluding otherwise.

## Background

- [`DEVBOX_ISSUES.md`](DEVBOX_ISSUES.md) Issue 2 — the original observation,
  the `[WATCHDOG]` log lines, and the (retracted) circumstantial sshd link.
- [`../reference/subsystems/syscalls/term.md`](../reference/subsystems/syscalls/term.md)
  → "Blocking stdin read" — the current-state reference, with the mermaid
  sequence diagram of this loop.
- [`../reference/subsystems/locking.md`](../reference/subsystems/locking.md)
  → "The per-syscall BKL opt-out list" gate 2 (the
  `term_state_lock`/`input_waker` non-compliance that blocks the BKL-free
  conversion of `read`'s Stdin arm) and the correctness rule "Mask
  IRQs/preemption per *attempt*, never across an unbounded wait".
- [`../reference/subsystems/smp-shared.md`](../reference/subsystems/smp-shared.md)
  M5d — the blocking-wait-drops-BKL work; documents that
  `schedule_blocking`/`blocking_relax` already drops preemption internally,
  so the wedge here is *not* an M5d regression.
- [`crates/akuma-exec/src/threading/mod.rs`](../../crates/akuma-exec/src/threading/mod.rs)
  → `disable_preemption` (`:1791`), `schedule_blocking` (`:3528`),
  `PREEMPTION_DISABLED_SINCE` (`:1752` — the watchdog counter).
- [`crates/akuma-exec/src/sync.rs`](../../crates/akuma-exec/src/sync.rs)
  → `KernelLock::held_by` (`:819`), `log_kernel_lock_stuck` (`:833-845`) —
  decode `owner=`/`waiter=` as core IDs (`aff0 + 1`).
- [`BKL_FINE_GRAINED_LOCKING_PLAN.md`](BKL_FINE_GRAINED_LOCKING_PLAN.md) §7.3a
  — the phase-7g lock-vs-atomic classification table, relevant to fix
  option 2.
- [`crates/akuma-exec/src/bkl.rs`](../../crates/akuma-exec/src/bkl.rs) →
  `reconcile_for_spsr` (`:344`), `enter_kernel`/`leave_kernel` (`:307`, `:320`)
  — the exact acquire/release/reconcile mechanics §9.1-§9.2 are built from.
- [`crates/akuma-ext2/src/ext2.rs`](../../crates/akuma-ext2/src/ext2.rs) →
  `read_state`/`write_state` (`:770-845`) and `state_hold_guard` (`:591`) — the
  already-correct per-attempt-guard precedent §9.4/§10 fix #1 mirrors.

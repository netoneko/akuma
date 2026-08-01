# BKL Phase 7 audit: is the BKL still load-bearing? — 2026-08-01

Phase 7 of [`BKL_FINE_GRAINED_LOCKING_PLAN.md`](BKL_FINE_GRAINED_LOCKING_PLAN.md) is
"BKL Removal & Hardening": drop the BKL from `rust_sync_el0_handler`, delete
`reconcile_for_spsr`, delete `KernelLock`, stress-test. This is the audit that was
supposed to green-light that, run after Phases 2–6 all landed default-on in
`smp-shared`.

**It does not green-light it.** Two independent findings, either of which alone is
disqualifying:

1. **The premise is stale by two carve-outs and one profiler fix.** "`irq/sched` at
   66–73% of remaining contention" is a pre-2026-07-31 number, superseded twice.
   Measured fresh at HEAD: **23.5%**. The BKL is *not* primarily a scheduler/IRQ lock.
2. **The BKL is still the only cross-core lock for the kernel's most pervasive
   pattern** — the 300-site `lookup_process() → &'static mut Process` family, whose
   entire safety argument is `with_irqs_disabled` (single-core mutual exclusion).
   Removing the BKL from syscall entry converts every one of those sites into a
   cross-core data race and a use-after-free window against `unregister_process`.
   **This half was already known** — `BKL_PROCESS_CARVE_OUT.md` §7 "(b)" named it as
   Phase 3's unblock condition and sized it at "218+ sites". §2.1 credits that and states
   what is actually new: it now gates the *whole phase* rather than one syscall family
   (§2.1), the documented safety argument covers self-teardown but not peer-core teardown
   (§2.1.1), and a viable migration pattern already ships in the M5b fault path, which
   makes it a large refactor rather than a research problem (§2.1.2).

Phase 7 therefore has to become a *prerequisites* phase, not a removal phase. §5
proposes the decomposition. §4 records three defects the audit turned up, one of them
root-caused and fixed here.

---

## 1. Correcting the premise

### 1.1 Where 66–73% came from, and why it is not current

`BKL_VFS_CARVE_OUT.md` §16 reported `irq/sched` at 66.3% (OFF side) / 73.2% (ON side).
Those numbers are real but were produced by a profiler with a **known over-crediting
bug**, fixed in §18 the same day: attribution was per-core, so a timer tick that
context-switched handed the incoming thread the `irq/sched` label, and a thread
preempted inside a long BKL-held syscall never re-entered the kernel to correct itself
— it ran that syscall's whole remainder labelled `irq/sched`. Because long excursions
are exactly the ones that get preempted, the artifact pooled in one bucket.

§18.4's matched A/B measured the correction: **88.8% → 23.0%**, and §18.5 states the
conclusion explicitly ("Does Phase 3's `irq/sched` premise survive? **No.**").

Two carve-outs then landed *after* those numbers — `no-bkl-drivers` (Phase 6) and, more
importantly, `netpoll_drain` (§20), which removed **57.2 percentage points** and cut
total workload spinning by 67.3%. `locking.md`'s "`irq/sched` alone was 66–73% of
remaining spin" sentence still quotes the pre-fix figure and is corrected by this
document.

### 1.2 Fresh baseline at HEAD (ff533a5), SMP=4, 2026-08-01

`SNAPSHOT=1 DISK=devbox.img MEMORY=4096 SMP=4`,
`release-smp-shared --features devbox-smoltcp,no-tests,bkl-profile`, the unmodified
`net4 → read4 → cp2 → rm` regimen, attribution restricted to the workload windows via
`analyze_workload.py --auto` (the §17.2/§18.4 method). Regimen wall-clock 90 s, **6/6
digests exact**, 0 PANIC / 0 WILD / 0 SPURIOUS / 0 stale dropped-window heals, 0
`[BKL] stuck`.

```
selection: auto: regimen execve T=31..112s -> windows t=40..120s
windows: 9  (w3 t=40s .. w11 t=120s)
total contended spins: 32908181   attributed: 32908181
```

| share | holder | tag | spins | has an inner lock? |
|---|---|---|---|---|
| 23.5% | `irq/sched` | 501 | 7,052,714 | **no** — alarm queue is `critical_section` (IRQ-only) |
| 22.3% | `execve` | 221 | 6,681,209 | **no** — `replace_image`'s destructive window |
| 13.3% | `clone` | 220 | 3,996,764 | **no** — fork steps 5–8 (`THREAD_CONTEXTS`, `register_process`) |
| 10.5% | `openat` | 56 | 3,137,769 | yes (carved; this is the residual outside the window) |
| 9.4% | `idle` | 502 | 2,828,169 | yes (`POOL`) |
| 8.3% | `netpoll_maint` | 504 | 2,485,014 | **no** — thread/process-table housekeeping |
| 6.3% | `ppoll` | 73 | 1,881,567 | partly (`EPOLL_TABLE`; but calls `smoltcp_net::poll()` BKL-held) |
| 2.2% | `nanosleep` | 101 | 669,291 | — |
| 1.3% | `rt_sigprocmask` | 135 | 377,662 | — |
| 1.1% | `netpoll_herd` | 507 | 320,529 | no (console UART, unlocked) |
| 0.9% | `read` | 63 | 272,881 | yes (carved; residual) |
| 0.5% | `accept` | 202 | 134,971 | yes (carved; residual) |

Cross-check: the `no-bkl-drivers` A/B from earlier the same day
(`/tmp/ab_drivers_{on,off}.log`) reads `irq/sched` 21.1% / 18.7% on the same regimen —
consistent with 23.5% under a profiler that perturbs by design, and consistent with
§18.4's 23.0%. Three independent runs agree; none is near 66%.

**Idle-boot floor** (same kernel, before the regimen): `idle` 36.2%, `irq/sched` 35.3%,
`netpoll_maint` 26.8%. With no userspace running, those three *are* the whole picture —
which is what the "BKL is now a scheduler lock" intuition was actually seeing. Under
load they are collectively 41%, and the process-lifecycle syscalls are 35.6%.

### 1.3 What the numbers actually say

Group the table by *nature of the holder* rather than by tag:

| group | share | why it is where it is |
|---|---|---|
| **Process lifecycle** (`execve` + `clone` + `nanosleep` + `rt_sigprocmask`) | **39.1%** | no inner lock exists; the BKL *is* the lock |
| **Kernel-thread / IRQ paths** (`irq/sched` + `idle` + `netpoll_maint` + `netpoll_herd`) | **42.3%** | mixed: `idle` has `POOL`, the rest do not |
| **Already-carved residual** (`openat` + `read` + `accept`) | **11.9%** | BKL-held prologues/epilogues outside the guard windows |
| **Carve candidates with an existing inner lock** (`ppoll`) | **6.3%** | `EPOLL_TABLE` exists; the BKL is redundant |

The single largest *actionable-by-carving* item is `ppoll` at 6.3%. Everything larger
needs a lock **built** first. That is the shape of Phase 7's real work.

---

## 2. What the BKL is still load-bearing for

"Load-bearing" here means: remove the BKL and this becomes a cross-core race, because
no other lock covers it. Each item was verified against the source, not inferred.

### 2.1 The process table — the blocker (already audited; this section only re-scopes it)

**This is not a new finding, and it should not be read as one.**
[`BKL_PROCESS_CARVE_OUT.md`](BKL_PROCESS_CARVE_OUT.md) §7 "(b)" already named it as the
thing that would unblock Phase 3 and already sized it:

> **(b) A real inner lock is added to the process table and `THREAD_CONTEXTS`.** This is
> Phase 3's original plan (`BKL_FINE_GRAINED_LOCKING_PLAN.md` §201–270:
> `PROCESS_TABLE_LOCK` + per-process `ProcessLock`). It was never built — the VFS
> carve-out succeeded *without* its planned lock hierarchy because VFS state already had
> locks, and process state does not. Building it is real design work (218+
> `lookup_process` sites to refactor), not a guard-and-measure cycle.

`lookup_process`'s own docstring (children.rs:323–334) states the hazard out loud too.
So the state of the world was known. What this audit adds is three things:

1. **The prerequisite is still unmet at HEAD** — nothing in Phases 4–6 touched it, and
   §9.2 explicitly (and correctly) declined to couple it to the `no-bkl-process` window.
2. **It is now the *removal* blocker, not just a fork/exec blocker.** Phase 3 needed it
   for one syscall family. Phase 7 wants the BKL gone from `rust_sync_el0_handler` for
   *every* syscall, which promotes it from "would unblock a carve-out" to "gates the
   whole phase."
3. **The documented safety argument is single-core reasoning and does not cover the
   cross-core case** — see §2.1.1, which is the genuinely new part.

The mechanics, for reference: `PROCESS_SLOTS` is an `AtomicPtr` array with `SLOT_STATES`
— lock-free for *finding* a slot. But every accessor then hands out a `&mut Process`
guarded by nothing but a local IRQ mask:

- `with_process(pid, |p: &mut Process| …)` — `with_irqs_disabled` only (table.rs:113)
- `get_process_ptr(pid) -> *mut Process` — same (table.rs:127)
- `lookup_process(pid) -> Option<&'static mut Process>` — process/children.rs:335
- `current_process() -> Option<&'static mut Process>` — process/children.rs:478
- `for_each_process` / `find_process` / `collect_pids` / `collect_process_info` — same

The docstring on `with_process` states the guarantee it provides: *"The callback runs
with IRQs disabled, guaranteeing the Process pointer is valid for the entire duration
(no other thread can free it)."* Under `smp-shared` that sentence is false on its own —
it is true only because the BKL means no peer core is in EL1 to call
`unregister_process`. `KernelLock`'s own docstring says so in as many words:

> This upgrades the kernel's pervasive single-core `with_irqs_disabled` invariant
> (mutual exclusion on one core only) into a genuine cross-core one, so the ~218 legacy
> `lookup_process() -> &'static mut Process` sites become correct without per-site
> changes.

And `fork_process` already works around it explicitly (process/mod.rs:1911): *"This
scan walks the PROCESS TABLE, which the `no-bkl-process` carve-out does NOT cover — the
table hands out `&'static mut Process` under nothing but a local IRQ mask, so the BKL is
its only cross-core lock."*

Scale: **300** call sites across `src/` and `crates/` (Phase 3 counted "218+"; the number
has grown, not shrunk), and **263** `with_irqs_disabled` uses overall. This is not a
carve-out; it is a redesign of the kernel's core ownership model.

### 2.1.1 The documented safety argument covers self-free, not peer-free

`lookup_process` (children.rs:332–334) justifies the 218+ legacy sites like this:

> This function exists for the 218+ legacy call sites in syscall handlers. Most are safe
> in practice because syscall handlers run in a single thread context and the process
> can't be freed during a syscall by its own thread.

That argument is sound for **self**-teardown, and it is why the pattern was never a bug on
single-core. It does not cover a **peer** freeing your `Process`, and the kernel does
exactly that in production:

| site | what it frees |
|---|---|
| `process/mod.rs:1116` (`kill_thread_group` sibling teardown) | `table::unregister_process(*sib_pid)` — *other* PIDs |
| `process/mod.rs:1209` (box kill) | `table::unregister_process(*pid)` over a collected list |
| `process/mod.rs:241` (`kill_process`) | another process by PID |
| `process/mod.rs:1464`, `:1606` (`return_to_kernel`, fault teardown) | self — the case the docstring covers |

`unregister_process` (table.rs:63–97) nulls the slot and returns the `Box`, whose drop
runs `Process::drop` → `UserAddressSpace::drop` and frees the page tables. So the
cross-core shape — core A holding a `&'static mut Process` from `lookup_process` while
core B runs sibling/box teardown for that same PID — is a real use-after-free whose only
guard today is the BKL. Under Phase 7's proposed removal it becomes reachable.

(Method note, since it nearly cost this audit a wrong conclusion: a first pass concluded
`unregister_process` was **test-only**. It was a truncation artifact — `grep -rn … src
crates | head` searches `src/` first, so `src/tests.rs` hits filled the window and every
`crates/` production caller was cut off. Re-run without `head`, or grep the crate
directly. This is the same class of near-miss as §9.1's empty grep for a lock that
existed.)

### 2.1.2 A migration shape already exists — 7e is less open-ended than Phase 3 assumed

The one genuinely encouraging finding. `lookup_process_shared` (children.rs:341–350) was
added for the M5b BKL-free page-fault path and does precisely what a post-BKL world needs:

> The shared-kernel-SMP (M5b) BKL-free page-fault path uses this instead of
> [`lookup_process`] so two cores faulting in different address spaces don't both
> materialize `&'static mut` to the same object (aliasing UB). Every address-space
> mutation the fault path needs is a `&self` method (`track_user_frame`,
> `track_page_table_frame`, `vm_with_regions`, `with_as_locked`) or a free function
> (`mmu::map_user_page*`); the actual cross-core mutual exclusion on the raw page-table
> writes comes from [`Process::as_lock`], not from `&mut` exclusivity.

That is §9.1's lesson already applied and shipped: `&mut` exclusivity replaced by
`&self` + an explicit inner lock, for one whole subsystem's worth of call sites. So 7e is
not "invent `PROCESS_TABLE_LOCK`" (which §9.2 correctly rejected as the wrong shape — a
new coarse lock). It is: **extend the `lookup_process_shared` + per-field-lock pattern to
the remaining accessors, and delete `lookup_process`/`current_process`.** Still large, but
it is an incremental refactor with a working precedent and it can proceed with the BKL
in place — not a research problem.

The residual hard part is the **free** path, which `as_lock` does not address: the
`Box::drop` in `unregister_process` needs deferred reclamation (an epoch/RCU scheme, or
the thread-slot cooldown pattern already used for `reclaim_terminated_slots`) so a peer
holding a reference cannot have it freed underneath. Phase 8 already floats RCU for
exactly this.

### 2.2 `THREAD_CONTEXTS`

`crates/akuma-exec/src/threading/mod.rs:1619` — a bare `[UnsafeCell<Context>;
MAX_THREADS]` with a hand-written `unsafe impl Sync`, accessed via
`get_context_mut(idx) -> *mut Context`. Its SAFETY comment reads:

```
/// 1. Each context is only modified by the scheduler with IRQs masked
/// 2. A context is only accessed when its thread is not running on any CPU
/// 3. We're single-CPU, so no concurrent access is possible
```

Clause 3 is false under `smp-shared`. Clause 2 is the real invariant and is probably
sufficient *if* `POOL`'s state transitions are what establish it — but that is currently
an unwritten argument, not an enforced one, and `fork_process` steps 5–8 write
`THREAD_CONTEXTS` for a thread that is not yet published, relying on the BKL for the
publication ordering (`locking.md` already records this: *"`THREAD_CONTEXTS` is an
unlocked `UnsafeCell`"*). Phase 7 needs clause 3 replaced with a proof or a lock.

### 2.3 The alarm queue and `critical_section`

`src/kernel_timer.rs`. `ALARM_QUEUE` is a `critical_section::Mutex<RefCell<[ScheduledWake;
QUEUE_SIZE]>>`, and this crate's `critical_section` implementation (kernel_timer.rs:323)
is **DAIF masking plus a process-global nesting counter**:

```rust
static CS_NESTING: AtomicU8 = AtomicU8::new(0);
static CS_SAVED_DAIF: AtomicU64 = AtomicU64::new(0);
```

That provides no cross-core exclusion at all, and the *global* nesting counter is worse
than merely useless under SMP — core A's `acquire` increments the same counter core B's
`release` decrements, so a concurrent pair can restore DAIF while a critical section is
still open. The BKL is what makes it safe today.

This matters more than its size suggests, because `on_timer_interrupt()` walks the alarm
queue **from every core's timer tick** (`src/timer.rs:69`). It is the substance of the
`irq/sched` 23.5%: 4 cores × 100 Hz = ~400 BKL acquire/release cycles per second whose
only job is protecting a 16-entry array that has no lock of its own. Scope is
contained — `critical_section` is used only in `kernel_timer.rs` — so this is the most
tractable item in this section.

### 2.4 `execve` → `replace_image`

`crates/akuma-exec/src/process/image.rs:29` and `:121`. The destructive window
(`UserAddressSpace::deactivate()`, address-space swap, `mmap_regions.clear()`, brk/entry
rewrite) is guarded by `LifecycleGuard::acquire()` — which is **not a lock**:
`lifecycle.rs:84` is `threading::disable_preemption()`, a *per-thread* counter. It stops
this thread being preempted; it does nothing about a peer core. Cross-core exclusion for
a half-built process is the BKL alone.

At 22.3% this is the #2 holder, and `BKL_PROCESS_CARVE_OUT.md` already calls it "the
single most dangerous carve-out target in the space."

### 2.5 `fork_process` steps 5–8

Unchanged from `locking.md`'s existing account: `ProcessInfo` write,
`get_saved_user_context`/`update_thread_context` (`THREAD_CONTEXTS`, §2.2),
`spawn_user_thread_initializing`, and `register_process` + `mark_thread_ready` (the
publication point). Phase 3 deliberately left these BKL-held because "those touch state
with no inner lock, where the BKL *is* the lock." That finding stands; `clone` is 13.3%.

### 2.6 `netpoll_maint`

`src/main.rs:1456–1517`, the async-main loop's top-of-iteration section: heartbeat
`safe_print!`, 30 s `dump_running_process_stats()` + `dump_thread_resume_points()`, and
— the frequent one — `reclaim_terminated_slots()` every 100 ms. The loop iterates once
per IRQ (it WFIs at the bottom), so it re-acquires the BKL ≥100×/s. §19.5 already
recommended against carving it, for the §2.1/§2.2 reason: it is process/thread-table
code with no fine-grained lock underneath.

---

## 3. What is merely habit

These hold the BKL but have a real inner lock underneath, so they are `no-bkl-*`-shaped
work rather than lock-design work:

| target | share | inner lock | note |
|---|---|---|---|
| `ppoll` (`src/syscall/poll.rs:880`) | 6.3% | `EPOLL_TABLE` `Spinlock`; per-fd primitives | Also calls `akuma_net::smoltcp_net::poll()` **BKL-held** at poll.rs:925 — the exact drain §20 just carved out of the BKL in async-main. Same precedent applies. |
| `epoll_*` (poll.rs:296/318/578) | below noise | `EPOLL_TABLE` | not measured; convert with `ppoll` |
| pipes (`src/syscall/pipe.rs:14`) | below noise | `PIPES` `Spinlock` | mind the SIGPIPE-under-lock rule (`locking.md`) |
| futex (`src/syscall/sync.rs:12`) | below noise | `FUTEX_WAITERS` `Spinlock` | |
| eventfd / timerfd (`:11`, `:13`) | below noise | own `Spinlock`s | |
| `openat`/`read`/`accept` residual | 11.9% | already carved | the BKL-held prologue/epilogue outside each guard window; worth a look at `sys_openat`'s 10.5% specifically |

`idle` (9.4%) sits between the two categories: `idle_halt`'s post-WFI bookkeeping only
touches `POOL` under its own IRQ-masked lock, and the code already says so
(threading/mod.rs:2651) — but it re-acquires the BKL to do it. That re-acquire looks
removable.

---

## 4. Defects found during the audit

### 4.1 FIXED — the BKL's own ticket accounting was broken

**Symptom.** The fresh baseline run logged **46 `[BKL] RECOVERED (reticket-skipped)`**
and 1 `reticket-owned`, in bursts of ~20, with **0 `advanced-lost`**. Contrary to
§19.4/§20.4 (where every recovery clustered pre-workload), these land *inside* the
workload windows — line 768 is in window w5 (t=60 s), 1129–1150 in w7 (t=80 s),
1479–1499 in w10 (t=110 s); the selected window is t=40–120 s.

**Root cause.** `KernelLock`'s invariant is "one `now_serving` advance per ticket handed
out." `acquire_no_ticket` (`crates/akuma-exec/src/sync.rs`) deliberately takes ownership
without allocating a serving slot — correct, because its caller
(`bkl::reconcile_for_spsr_no_ticket`, the BKL-free EL0-preempt scheduler path) never
called `enter_kernel`. But the thread it reconciles *into* is ordinary EL1 code, and
that thread's eventual EL1→EL0 return goes through the **normal** epilogue
(`reconcile_for_spsr` → `release`), which advances `now_serving` unconditionally for the
owner. So each such hold advanced without allocating, driving `now_serving` **ahead of**
`next_ticket`. From then on every contended acquirer took a ticket already behind
`now_serving`, hit the "skipped" branch, and re-ticketed — one recovery per acquire
until `next_ticket` caught up, which is exactly the observed burst shape. For the length
of a burst the fair FIFO lock degrades to an unfair test-and-set.

This is the mirror image of the 2026-07-24 fix. That one removed a `next_ticket` advance
with no matching `now_serving` advance; the replacement left a `now_serving` advance with
no matching allocation. The sign flipped and the symptom changed from a hard deadlock
(`owner == 0`, all cores spinning) to a self-healing storm, which is why it survived —
the self-heal made it look benign. `sync.rs:490`'s "Root cause not yet pinned" note
describes the opposite-sign shape (`next_ticket == now_serving + 5`); that one is
separate and still open.

**Fix.** `acquire_no_ticket` bumps `next_ticket` *after* winning the ownership CAS (so a
losing CAS allocates nothing). This is also the right FIFO semantics: if a waiter was
sitting at `now_serving` when the barge came in, its own CAS just failed and it
re-ticketed to the tail (`reticket-owned`), leaving that slot vacant — so the barger's
release *should* advance past it. With no queue, one slot is allocated and consumed and
the counters return to equal.

**Tests.** Host regression
`sync::tests::kernel_lock_no_ticket_acquire_release_stays_balanced` (2000 rounds,
asserts `next_ticket == now_serving` after each no-ticket hold *and* after an
interleaved normal ticketed excursion; fails on round 0 pre-fix with `left: 0, right:
1`). Boot self-test `test_no_bkl_ticket_recoveries` (`src/process_tests.rs`, runs last
in the suite like the phantom-SVC tripwire) asserts the new
`akuma_exec::sync::kernel_lock_recoveries()` counter is 0 — every `[BKL] RECOVERED` path
now increments it, so a future pairing break is a test failure rather than log lines
nobody greps.

### 4.2 `execve` prints five UART lines per exec inside the BKL-held destructive window

`process/image.rs:31–54` has five unconditional `(runtime().print_str)("[FORK-DBG] …")`
calls, three of them *inside* the `LifecycleGuard` destructive window, and the console
(`src/console.rs:60`) is a bare `static UART` with no lock. They are live — the baseline
log is full of `[FORK-DBG] replace_image: swapping AS`. On the #2 BKL holder (22.3%),
under a regimen whose whole point is fork/exec churn. Left in place, not fixed here:
they are debug scaffolding for the fork-corruption work and removing them is the fork
owner's call, but they should not be in a Phase 7 baseline.

(Note: `grep` reports 0 matches for `FORK-DBG` in these logs — the serial capture
contains NUL bytes so `grep` treats it as binary and suppresses output. Use `awk`.
This tripped up part of this audit and is worth knowing for anyone reading these logs.)

### 4.3 Stale SAFETY comments asserting single-core

`THREAD_CONTEXTS`'s "3. We're single-CPU, so no concurrent access is possible"
(§2.2) and `with_process`'s "guaranteeing the Process pointer is valid" (§2.1) are both
false as written under `smp-shared`. They are *currently harmless* because the BKL makes
them true, which is precisely why they are dangerous: they are the documentation someone
will read while removing the BKL. Not edited here — they should be corrected as part of
whichever sub-phase gives those structures real locks, so the comment and the mechanism
change together.

### 4.4 Checked and NOT a defect: `schedule_blocking`

`schedule_blocking` (threading/mod.rs:3001) parks in a `wfi` loop with no BKL drop,
which looks like a violation of `locking.md`'s "never hold a lock across a blocking
wait" — `ppoll` calls it once per poll iteration. Traced through: it marks itself
WAITING and triggers a voluntary SGI, and `schedule_indices` (threading/mod.rs:2298)
falls back to **this core's idle thread** when no non-idle thread is READY, so the
switch does happen; the idle thread then drops the BKL at `idle_halt`'s own
`leave_kernel()` before its WFI. The BKL is released.

One residual: if the SGI's `POOL.try_lock()` misses (threading/mod.rs:2732, a documented
best-effort skip), no switch occurs and the caller reaches its own `wfi` still holding
the BKL until the next tick. Bounded at ~10 ms, rare, and not observed — noted so a
future BKL-across-WFI hunt does not have to re-derive it.

---

## 5. Recommended decomposition

Phase 7 as one step is not executable. Split it, evidence-led, cheapest-first — and note
that the first three sub-phases together address ~30% of measured contention without
touching the process table at all:

**7a — the alarm queue and `critical_section` (§2.3).** Give `ALARM_QUEUE` a real
`Spinlock` and make the `critical_section` nesting counter per-core (or drop the
`critical_section` dependency and use the kernel's own `IrqGuard` + a Spinlock). Then
the timer tick's `on_timer_interrupt` no longer needs the BKL. Smallest blast radius of
anything here (one file, ~40 lines), and it is the substance of the largest single tag.
Deliverable: `dispatch_irq` for IRQ 27 runs BKL-free; `irq/sched` share drops in a
same-binary A/B.

**7b — `ppoll`/`epoll` carve-out (§3).** Standard `no-bkl-*` work with an existing inner
lock, plus moving the BKL-held `smoltcp_net::poll()` at poll.rs:925 into a dropped
window (the §20 precedent). 6.3%, and the playbook is written.

**7c — the already-carved residual (§3, 11.9%).** Re-audit `sys_openat`'s guard
placement specifically: 10.5% for a converted syscall's prologue/epilogue is high enough
that either the window starts too late or the re-acquire is costing more than expected.
Measurement first, not code.

**7d — `THREAD_CONTEXTS` ownership proof (§2.2).** Either establish that `POOL`'s state
machine already guarantees "not running on any CPU" (then the fix is a corrected SAFETY
comment plus a host test over the state transitions), or add per-slot ownership. Cheap
if the former, and it is a prerequisite for anything touching `clone`.

**7e — process-table locking (§2.1).** The real blocker, but §2.1.2 downgrades it from
"research task" to "large incremental refactor with a shipped precedent." Two separable
halves:

- *Access* — extend `lookup_process_shared`'s `&self` + `as_lock`/`vm_lock` pattern
  (children.rs:341, already carrying the M5b BKL-free fault path) to the remaining
  accessors and delete `lookup_process`/`current_process`. 300 sites, mechanical per site,
  proceeds with the BKL in place, and is worth doing on its own merits — §9.2 said so.
  Do **not** build `PROCESS_TABLE_LOCK`; §9.2 already rejected that shape.
- *Free* — deferred reclamation for `unregister_process`'s `Box::drop`, which no existing
  inner lock covers (§2.1.1: peer-core sibling/box teardown frees other PIDs' `Process`).
  Epoch/RCU, or the time-cooldown pattern `reclaim_terminated_slots` already uses for
  thread slots. This half is the genuinely new design work.

**Nothing about removing the BKL from syscall entry should be attempted before both
halves land.**

**7f — wither the BKL; do not remove it.** The plan's original tasks 1–3 are replaced by a
per-syscall opt-in list: `rust_sync_el0_handler` goes from "always acquire" to "acquire
unless this syscall is converted," seeded **empty** (byte-identical behaviour), then
syscalls move across one at a time. Bisectable, keeps the per-syscall kill switch every
prior phase relied on, and makes `KernelLock`/`reconcile_for_spsr`/the ledger/the five
guards *provably* dead code at the end — so deleting them is bookkeeping, not a
behavioural change. Critically, the ledger and `reconcile_for_spsr` must **survive** the
whole traversal: a converted syscall is a permanently-open dropped window, and the
ledger's invariant is what makes the mixed state safe.

Rationale, and the remaining conversion surface (14 untouched syscall families + ~13
leftover `fs` syscalls, none above the noise floor — so no attribution signal to guide or
validate them), are written up in
[`BKL_FINE_GRAINED_LOCKING_PLAN.md`](BKL_FINE_GRAINED_LOCKING_PLAN.md) §7.3, which is now
the canonical statement of this phase. `execve` (§2.4) and `clone` (§2.5) are the last two
across, after 7e.

**7g — audit which locks can become atomics, before the deletion.** Slotted between 7f's
traversal and the BKL infrastructure's removal. Every hardening fix this campaign has
made is a *discipline* fix (mask IRQs, shorten the span, hoist the user copy out), and
discipline is a standing obligation that every future call site has to honour. A lock that
becomes a plain atomic stops being an obligation: it cannot AB-BA against the BKL, cannot
be held across a blocking wait, and leaves the load-bearing inventory. 7f tranche 3 found
the first instance by accident — `UTC_OFFSET_US` was a `Spinlock<Option<u64>>` guarding
one scalar, and became an `AtomicU64`. The classification table and the "don't do this for
speed" caveat are in
[`BKL_FINE_GRAINED_LOCKING_PLAN.md`](BKL_FINE_GRAINED_LOCKING_PLAN.md) §7.3a.

A note on ordering rationale: this list deliberately does *not* follow contention rank.
`execve` at 22.3% outranks everything in 7a–7d, but it has no inner lock and is
documented as the most dangerous target in the campaign; converting it before the
process table has a locking story would be building on the thing that needs replacing.

---

## 6. Verification of what changed here

- **Host**: `cargo test -p akuma-exec --target aarch64-apple-darwin` — **156 passed, 0
  failed**, including the new `kernel_lock_no_ticket_acquire_release_stays_balanced` and
  the pre-existing `bkl_model::tests::kernel_lock_concurrent_stress` (real `KernelLock`
  under `std::thread` contention with a watchdog).
- **Clippy**: `cargo clippy --profile release-smp-shared --features
  devbox-smoltcp,bkl-profile` clean (the one pre-existing `needless_range_loop` in
  `src/bkl_profile.rs:111` is fixed here too).
- **Boot self-test suite, SMP=4** (`release-smp-shared --features smp-shared`,
  `MEMORY=2048 SMP=4`): **238 PASSED, 2 FAILED**, both pre-existing and unrelated
  (`PermissionDenied -> EPERM` errno mapping, and `stp_xzr_ec15_handler_fires`, which
  self-documents as QEMU-dependent) — verified identical in `selftest.log` (2026-08-01,
  pre-change) and `madvise_boot_suite_smp2.log` (2026-07-25). 0 PANIC / 0 WILD / 0
  SPURIOUS-SVC. The new `no_bkl_ticket_recoveries` and the existing
  `no_spurious_svc_traps` both PASSED.
- **Boot + regimen at SMP=4**: see §6.1.

### 6.1 Same-regimen re-run with the ticket fix

Same kernel config and regimen as §1.2, same host, back to back with the baseline.

| | baseline (§1.2) | with the ticket fix |
|---|---|---|
| `[BKL] RECOVERED` | **46** (45 `reticket-skipped`, 1 `reticket-owned`) | **0** |
| `[BKL] stuck` | 0 | 0 |
| PANIC / WILD / SPURIOUS | 0 / 0 / 0 | 0 / 0 / 0 |
| stale dropped-window heals | 0 | 0 |
| digests (4 net + 2 cp) | 6/6 exact | 6/6 exact |
| regimen wall-clock | 90 s | 90 s |
| workload windows | 9 (t=40–120 s) | 9 (t=480–560 s) |

Attribution on the fixed side, for comparison with §1.2's table:

| share | holder | tag |
|---|---|---|
| 22.4% | `execve` | 221 |
| 20.8% | `irq/sched` | 501 |
| 13.0% | `ppoll` | 73 |
| 11.3% | `netpoll_maint` | 504 |
| 10.2% | `clone` | 220 |
| 10.0% | `openat` | 56 |
| 9.0% | `idle` | 502 |

The ranking and the §1.3 grouping are stable across both runs; `ppoll` is the one tag
that moves materially (6.3% → 13.0%), so treat it as a range, not a point estimate.

**No claim is made about total contention.** Total attributed spins read 32.9M (baseline)
vs 41.4M (fixed), but the fixed boot idled ~470 s before its regimen while the baseline
started at ~31 s, and the campaign's own rule applies: never compare absolute spin counts
across sessions, only shares and ranks within one run. The defensible result here is
**46 → 0 recoveries with every other stability signal and all six digests unchanged** —
i.e. the fix removes the FIFO corruption without altering behaviour or throughput.

---

## Background

- [`BKL_FINE_GRAINED_LOCKING_PLAN.md`](BKL_FINE_GRAINED_LOCKING_PLAN.md) — the phased
  plan whose Phase 7 this audits.
- [`BKL_VFS_CARVE_OUT.md`](BKL_VFS_CARVE_OUT.md) — §16 is where 66–73% came from, §18
  is the thread-scoped attribution fix that superseded it, §19–§20 the `netpoll_drain`
  decomposition and carve.
- [`BKL_PROCESS_CARVE_OUT.md`](BKL_PROCESS_CARVE_OUT.md) — the `clone`/`execve` audit
  this reconfirms; §§1–8 are why no carve-out was possible, §9 what landed.
- [`BKL_MM_CARVE_OUT.md`](BKL_MM_CARVE_OUT.md),
  [`BKL_DRIVERS_CARVE_OUT.md`](BKL_DRIVERS_CARVE_OUT.md) — Phases 5 and 6; the latter's
  §2 is what deferred the IRQ-handler goal to this phase.
- [`../reference/subsystems/locking.md`](../reference/subsystems/locking.md) — the
  distilled playbook and syscall→lock map; §1.1 above corrects its `irq/sched` figure.
- [`../runbooks/debug-smp.md`](../runbooks/debug-smp.md) — BKL wedge procedures.

# Signal Hell — Failure Summary (trim0.log)

Captured from a boot run. All failures below need to be fixed before acceptance testing is meaningful.

---

## 1. Thread-group kill / exit_group (6 failures) — FIXED 2026-05-26

**Status: RESOLVED.** All 6 now PASS (confirmed on a real boot, trim4.log). The
original framing ("threads in a group not observing kill/exit signals") was
WRONG — the production kill/exit_group path is sound. These were all broken
*test harness* code, in two distinct flavours.

```
kill_thread_group_terminates_before_cleanup   s1=0 s2=0 s3=0 — expected TERMINATED=3
exit_group_kills_siblings_before_close_all     s1=0 s2=0 leader=0
kill_thread_group_clears_thread_id             sib_exists=false sib_state=0 leader_tid=Some(Some(128))
zombie_process_unregistered_after_return_to_kernel  reg=true exited=false dropped=true gone=true
sys_kill_sets_interrupted_flag                 not interrupted
goroutine_kill_does_not_kill_leader            alive=false name=false !exited=false sib_gone=false
```

### Cause A — fake TIDs the state array cannot represent (3 tests)

`terminates_before_cleanup`, `exit_group_kills_siblings`, `clears_thread_id`
assigned sibling `thread_id = 128/129/130` and asserted
`get_thread_state(tid) == TERMINATED`. `THREAD_STATES` is a fixed 64-slot array;
`mark_thread_terminated(idx)` / `get_thread_state(idx)` both early-return for
`idx >= MAX_THREADS (64)`, so the slot can never be observed. Commit `8624aab`
bumped these from `28/29/30` (valid slots, passing) to `128/129/130` to stop
clobbering real threads (the `fake_thread_ids_safe` concern) — correct intent,
but it made the assertions impossible.

**Fix:** new test-only helper `claim_test_thread_slots(n)` /
`release_test_thread_slot(idx)` in `crates/akuma-exec/src/threading/mod.rs`
atomically claims genuinely-FREE slots (parked INITIALIZING, never dispatched by
the scheduler or `spawn_*`). The 3 tests now use claimed slots — observable AND
collision-free.

### Cause B — tests asserting behavior the code never had (3 tests)

- `sys_kill_sets_interrupted_flag`: `interrupt_thread(tid)` sets the flag on
  `get_channel(tid)`, but the boot test thread has no channel registered, so it
  was a silent no-op. Fix: register a temporary channel for the current thread.
  (Also fixed a `!0u64`→`0u64` cleanup-mask bug, same class as Cluster 2.)
- `zombie_process_unregistered_after_return_to_kernel`: expected
  `kill_thread_group(sibling)` to mark a bystander `exited` — it never does that.
  Real `sys_exit_group` marks the *caller* Zombie, then `kill_thread_group` reaps
  the *other* members. Rewritten to model that.
- `goroutine_kill_does_not_kill_leader`: called `kill_thread_group` (the
  exit_group/crash group-kill primitive) but asserted the leader survives.
  `kill_thread_group` correctly tears the whole group down. Real
  `return_to_kernel` only calls it when the exiting thread OWNS the address space
  (`!is_shared` — the leader); a goroutine's normal exit (shared) must not.
  Rewritten to model the `is_shared` gate.

### New regression test (crush goroutine-coordination guard)

`test_blocked_sibling_woken_by_cross_thread_signal` was added to replicate the
crush stall (goroutines failing to coordinate an LLM response): a *different*
thread delivers SIGURG via the exact `sys_kill` sequence (interrupt → pend →
wake) to a blocked sibling, asserting all three coordination effects — channel
interrupted (EINTR), signal pending, and WAITING→READY.

**Result: it PASSES** (trim4.log). So the cross-thread wake/interrupt mechanism
is intact in isolation — crush's stall is NOT in this layer. The test now stands
as a regression guard and rules this path out; the crush hang must be elsewhere
(candidates: SGI/scheduler re-dispatch, epoll_pwait re-arm, or a lock held by a
terminated thread). Separate investigation.

---

## 2. Pending signal bitmask (4 failures) — FIXED 2026-05-26

**Status: RESOLVED.** All four now PASS (confirmed on a real boot run, trim2.log).

```
pending_signal_bitmask_multiple
  first=15 taken=None second=15
  Signal 15 is pending but take() returns None.

pending_signal_take_clears_one
  None None None None
  take() returns None for all four queued signals.

pending_signal_mask_blocks
  taken=Some(2)
  A masked signal (2/SIGINT) is being delivered despite the mask.

pend_signal_or_semantics
  has_15=true taken=None has_23=false
  Signal 15 shows as pending but cannot be taken; signal 23 not recorded at all.
```

**The original root-cause hypothesis (split/non-atomic bitmask, off-by-one
numbering) was WRONG.** The kernel implementation in
`crates/akuma-exec/src/threading/mod.rs` (`pend_signal_for_thread`,
`peek_pending_signal`, `take_pending_signal`) is correct. This was a **test
bug**, not a kernel regression.

**Actual root cause:** `take_pending_signal(mask)` treats `mask` as the set of
*blocked* signals — `deliverable = pending & (!mask | force_bits)`. So:
- `0u64`  = block nothing = "take any pending signal"
- `!0u64` = block *everything* = nothing deliverable except SIGKILL/SIGSTOP

The four failing tests in `src/process_tests.rs` called
`take_pending_signal(!0u64)` while *intending* "take any signal." Every take
returned `None`, and the un-taken bits then leaked into the next test — which
is why `mask_blocks` reported the nonsensical `taken=Some(2)` (a leftover
SIGINT pended by the preceding `take_clears_one` test).

Three independent proofs the impl is right and the tests were wrong:
1. `src/syscall/signal.rs:227` documents *"take_pending_signal takes a mask of
   BLOCKED signals"* and passes `!wait_mask`.
2. `src/tests.rs:8285` — a sibling test that already PASSES — uses
   `take_pending_signal(0u64)` with the same primitives.
3. `test_sigkill_bypasses_mask` deliberately passes `!0u64` to verify SIGKILL
   survives a full mask, confirming `!0u64` means "block all."

**Why the doc said these "were passing at some point":** commit `230ddfa`
("attempt to fix signals") migrated the single-slot `AtomicU32` to the
`AtomicU64` bitmask and, in the same commit, *replaced* the old
`test_pending_signal_is_single_slot` (which passed) with these new tests —
already calling `!0u64`. So these specific tests never passed post-migration;
the passing test they replaced is gone.

**Fix:** changed `!0u64` → `0u64` in the four tests (`bitmask_multiple`,
`take_clears_one`, `mask_blocks` cleanup, `or_semantics`) in
`src/process_tests.rs`. `test_sigkill_bypasses_mask` left untouched.

**Note on `current_thread_id()`:** a red herring for these failures. It is a
one-line alias that already calls the identical `get_current_thread_register()`
(`tpidrro_el0`) used internally by `take_pending_signal`, so pend and take
always hit the same slot. Deprecating it would fix nothing here.

**Implication for Cluster 1:** the original suggestion was that the
thread-group kill tests would "self-resolve once signals deliver correctly."
Since the signal primitives were never actually broken, they will NOT
self-resolve — confirmed by trim2.log, where all 6 still fail. Treat Cluster 1
as an independent bug.

---

## 3. STP instruction decoder — `stp_xzr_misroute_decode` FIXED 2026-05-26

**Status of `stp_xzr_misroute_decode`: RESOLVED** (test-only fix). The original
framing below ("decoder computing wrong offsets / rejecting valid encodings")
was WRONG. `decode_stp_xzr_xzr` in `src/exceptions.rs:719` is correct per the
ARM64 STP signed-offset encoding (imm7 in bits[21:15], Rt in bits[4:0], proper
sign-extension). This was a **test bug**: 5 of the 7 hand-assembled instruction
words in `src/process_tests.rs` did not encode the instruction their label
claimed. The 2 well-formed cases (`[x0]`, `[x0,#0x10]`) always passed, which
already proved the decoder logic.

```
stp_xzr_misroute_decode (original failure log)
  stp xzr,xzr,[x0,#0x70]  instr=0xa90e7c1f  got=Some((0,224))  want=(0,112)
  stp xzr,xzr,[x3,#0x20]  instr=0xa9027c63  got=None           want=(3,32)
  stp xzr,xzr,[x0,#-0x8]  instr=0xa97f7c1f  got=None           want=(0,-8)
  stp xzr,xzr,[x0,#-0x10] instr=0xa97e7c1f  got=None           want=(0,-16)
  stp xzr,xzr,[x0,#-0x200] instr=0xa9407c1f got=None           want=(0,-512)
```

What each malformed word actually was, and the corrected constant:

| label          | old (wrong) word | defect in the word                  | corrected word |
|----------------|------------------|-------------------------------------|----------------|
| `[x0,#0x70]`   | `0xa90e7c1f`     | imm7=28 (→224), should be 14         | `0xa9077c1f`   |
| `[x3,#0x20]`   | `0xa9027c63`     | Rt=x3 in bits[4:0], should be xzr    | `0xa9027c7f`   |
| `[x0,#-0x8]`   | `0xa97f7c1f`     | imm7 placed in bits[22:16] not [21:15] | `0xa93ffc1f` |
| `[x0,#-0x10]`  | `0xa97e7c1f`     | same bit-position error              | `0xa93f7c1f`   |
| `[x0,#-0x200]` | `0xa9407c1f`     | same bit-position error              | `0xa9207c1f`   |

The "offset doubled (224 = 0x70*2)" symptom was a coincidence of the wrong word,
not a scale bug; the decoder never doubles anything. Fixed by replacing the 5
constants in `src/process_tests.rs`. `cargo check` clean.

### `stp_xzr_ec15_handler_fires` — STILL FAILING (separate, real issue)

NOT a decoder bug and NOT fixed. This is a runtime/QEMU concern: QEMU's TCG is
generating EC=0x25 (data abort) for `stp xzr,xzr` on a PROT_NONE page instead of
EC=0x15, so the EC=0x15 emulation path is never exercised. The handler may be
registered under the wrong EC, or the instruction is simply handled by the
EC=0x25 demand-pager before the EC=0x15 path is reached. Needs its own
investigation — left untouched.

```
stp_xzr_ec15_handler_fires
  EC=0x15 STP handler never fired.
  QEMU is generating EC=0x25 for this instruction class instead of EC=0x15.
```

---

## 4. Thread safety — `fake_thread_ids_safe` FIXED 2026-05-26

**Status: RESOLVED.** Passes in trim5.log. Again a test-assumption bug, not real
corruption.

```
fake_thread_ids_safe FAILED: system threads corrupted   (slots 1,2,3 had state 0 = FREE)
```

The test demanded slots 0-3 be READY/RUNNING, assuming live system threads
occupy slots 1-3. But `process_tests::run_all_tests()` runs at `main.rs:619`,
while the SSH/HTTP service threads aren't spawned until `main.rs:651`/`810` —
*after* the test suite. So at test time slots 1-3 are legitimately FREE. The
test never passed (added in `5920862`).

**Fix:** rewrote the assertion to guard what actually matters — that the
fake-TID test harness (the kill_thread_group tests, which run earlier) never
clobbered a reserved system slot:
- slot 0 (idle thread) must be READY/RUNNING (always live),
- slots 1-3 must be FREE (unspawned, normal) or a live state; TERMINATED /
  INITIALIZING in a reserved slot would flag corruption.

Structurally reinforced by the Cluster 1 fix: `claim_test_thread_slots` only
claims FREE slots in `reserved_threads..MAX_THREADS` (8..64), so the test harness
can never touch system slots 0-7.

---

## 5. Minor / lower priority

```
FS errno mapping
  PermissionDenied → EPERM: got -13, expected -1
  fs_error_to_errno_mapping is returning the raw errno value (-13) instead of -1.
  (Standard Linux syscall convention: return -errno, but test may expect the positive errno
  to be negated at the syscall boundary, not inside the FS layer.)

procfs stdout
  procfs stdout missing expected content (got 0 bytes)
  A procfs read returned empty. Low priority unless something depends on /proc for signals.
```

---

## Suggested attack order

1. ~~**Pending signal bitmask**~~ — DONE (2026-05-26, test-only fix; see Cluster 2). The "underlies most of the kill tests" premise was false; the primitives were never broken.
2. ~~**Thread-group kill / exit_group**~~ — DONE (2026-05-26, test-harness fixes; see Cluster 1). All 6 pass in trim4.log. New guard test `blocked_sibling_woken_by_cross_thread_signal` added and passing — rules out the cross-thread wake path as the crush stall.
3. ~~**fake_thread_ids_safe**~~ — DONE (2026-05-26, test-assumption fix; see Cluster 4). System threads spawn after the test suite, so FREE slots are expected, not corruption.
4. ~~**STP decoder**~~ — `stp_xzr_misroute_decode` DONE (2026-05-26, test-only fix; the decoder was correct, the test words were malformed). `stp_xzr_ec15_handler_fires` remains: real EC 0x25-vs-0x15 runtime issue, separate from the decoder.
5. **FS errno / procfs** — clean up after the above.

---

# Confirmed crush bug & fix: `exit_group(0)` reported as `-9` (2026-05-26)

**Symptom (interactive):**
```
akuma:/tmp> crush
[exit code: -9]
```
crush ran, the LLM responded, then on exit the shell reported `-9` (killed by
signal 9) even though no SIGKILL was ever sent (grep of the boot log shows zero
`sig=9` / SIGKILL events) and a thread reached `[exit_group] code=0`.

**Root cause:** `kill_thread_group` stamped a **hardcoded `-9`** on every sibling
channel (`channel.set_exited(-9)`). When a Go **goroutine** calls `exit_group(0)`,
the thread-group **leader** is one of the "siblings" torn down — and the leader's
I/O channel is exactly the `Arc<ProcessChannel>` the interactive shell reads for
the exit status (`src/shell/mod.rs:294` → `channel.exit_code()`). So a clean
`exit_group(0)` was overwritten to `-9` before the shell read it. (`set_exited`
is last-writer-wins; `kill_thread_group` had no `has_exited()` guard, unlike
`teardown_forked_process_thread_group`.)

**Fix:** `kill_thread_group(my_pid, l0_phys, exit_code)` now takes the group's
real exit code and applies it only when the channel hasn't already recorded one
(`if !channel.has_exited()`). Callers pass the right code: exit_group → `code`,
SIGKILL → `-9`, EC=0x25 fault → `-14`, fork-subtree teardown → `137`,
return_to_kernel(_from_fault) → `exit_code`.

**Status: FIXED, confirmed on trim8.log.** `test_kill_thread_group_preserves_exit_code`
PASSES (goroutine `exit_group(0)` leaves the leader's channel reporting `0`, not
`-9`). The existing `test_kill_thread_group_sets_child_channel_exited` was updated
to assert the caller's code propagates (now passes `137`, asserts `137`) instead
of the old hardcoded `-9`. The cross-thread coordination guard
`test_blocked_sibling_woken_by_cross_thread_signal` also passes.

This explains the *exit status*. It does NOT by itself explain the runtime
slow-down below — that's a separate, still-open problem.

**Note (test infra):** the `parallel_processes` threading test was made robust in
the same pass — it required *both* the `ps` and `kthreads` liveness checks, but
`ps` spawns a slow busybox subprocess that races short-lived `/bin/hello`, so it
now accepts *either* (`ps_done || kthreads_done`). Unrelated to the kill path; it
was halting the boot test suite before `process_tests` could run.

---

# Crush goroutine-coordination stall — working theories (2026-05-26)

This is the *real* live bug behind the whole "signal hell" investigation: `crush`
(a multi-goroutine Go LLM client) cannot coordinate its goroutines to receive a
response from the LLM — it makes progress for a while then effectively stalls.
None of the unit-test clusters above reproduce it; they were broken test
harnesses. This section collects the evidence and ranked theories so the next
session can start from the right place.

## What's already ruled OUT

- **Pending-signal bitmask / mask semantics** (Cluster 2) — primitives were
  always correct; only the tests were wrong.
- **Cross-thread wake/interrupt mechanism** — the new guard test
  `test_blocked_sibling_woken_by_cross_thread_signal` PASSES: a signal delivered
  from another thread via the `sys_kill` sequence (interrupt → pend → wake)
  correctly sets the channel interrupted flag, pends the signal, and flips the
  target WAITING→READY. So the *primitive* layer of goroutine preemption works.
- **Futex key inconsistency across CLONE_VM threads** — futexes are keyed
  `(read_current_pid(), uaddr)`; CLONE_VM threads share `PROCESS_INFO_ADDR`, so
  all threads in the group resolve the same namespace. (One caveat survives as
  Theory 1.)

## Evidence — `crush` (PID 102) PSTATS, trim3.log

```
t=10.89s  5359 syscalls  in_kernel=38476ms   futex=226(21798ms)  nanosleep=320(8874ms)   epoll_pwait=347(4900ms)
t=40.89s 16728 syscalls  in_kernel=146000ms  epoll_pwait=1891(53517ms)  futex=291(50041ms)  nanosleep=1092(37432ms)
t=70.91s 25341 syscalls  in_kernel=272199ms  epoll_pwait=3508(120602ms) futex=315(79749ms)  nanosleep=1897(66775ms)
```

Shape of the data:
- **futex call count is nearly flat (226→291→315) while futex wait time balloons
  (21.8s→50s→79.7s).** A few waiters are parked for very long and never woken —
  classic lost-wakeup signature.
- **nanosleep grows steadily (320→1092→1897 calls).** Go's runtime falls back to
  timed sleeps when event wakeups don't arrive — a *symptom* of a missed wakeup,
  not a root cause.
- **epoll_pwait time grows large** but `read` keeps trickling (255→312→328), so
  the netpoller isn't fully dead — readiness is delivered sometimes.
- `in_kernel` (272s) >> wall (71s) is just per-thread summed blocking time, not
  itself a bug.

## Ranked theories

**Theory 1 — lost futex wakeup via the `tgid = 0` fallback (LEAD).**
`futex_key_tgid` is `read_current_pid().unwrap_or(0)` (`src/syscall/sync.rs:17`).
If `read_current_pid()` ever returns `None` for one side of a wait/wake pair
(e.g. TTBR0 momentarily on boot tables, or an edge in the info-page read), that
operation silently keys `(0, uaddr)` and misses the real `(leader_pid, uaddr)`
queue — the waiter is never woken. Flat futex-call / ballooning futex-wait fits
this exactly.
*Probe:* log `(key_tgid, uaddr)` on every FUTEX_WAIT enqueue and FUTEX_WAKE; look
for a WAKE that finds an empty queue while a WAIT sits under a different key.
Assert `read_current_pid()` is never `None` on the futex path (treat None as a
hard error, not `0`).

**Theory 2 — epoll readiness edge lost / not re-armed.**
crush's netpoller blocks in `epoll_pwait` for the socket carrying the LLM
response. If a readiness edge isn't latched (or a level-triggered fd isn't
re-reported after a partial read), the goroutine that should read the response
never wakes. Reads do still trickle, so this is partial, not total.
*Probe:* instrument `epoll_pwait` return path — how many events, which fds, and
whether a known-readable socket failed to be reported.

**Theory 3 — woken thread not promptly dispatched (SGI/scheduler latency).**
`wake()` sets WAITING→READY and fires `trigger_sgi(0)`, but the guard test only
proved the *state transition*, not that the scheduler actually re-dispatches the
slot quickly. If dispatch lags (preemption disabled in a long critical section,
or the round-robin skips the slot), wakeups arrive late and goroutines back off
into nanosleep.
*Probe:* measure latency from `wake()` to the target thread actually running.

**Theory 4 — orphaned lock held by a terminated goroutine.**
If a goroutine thread terminated while holding a kernel lock (futex bucket,
EPOLL_TABLE), peers block forever. `is_thread_terminated()` orphan detection
exists but may not cover every lock.
*Probe:* on long futex/epoll blocks, check whether the lock's last holder is a
TERMINATED/FREE tid.

Theories 1 and 2 best fit the PSTATS; start there.

---

# Runtime degradation, scheduling & epoll (2026-05-26, trim6.log)

This section drills into *why* crush slows to a crawl after the LLM starts
issuing tool calls. It is the in-depth companion to Theories 2 and 3 above. The
short version: **epoll readiness latency is coupled to the round-robin scheduler
quantum, and Go's runtime reacts to slow wakeups by spinning, which adds more
runnable threads, which lengthens the quantum cycle — a positive-feedback
degradation loop.**

## The two relevant constants (both 10 ms)

- `TIMER_INTERVAL_US = 10_000` (`src/config.rs:329`) — preemptive round-robin
  tick. With N runnable threads, a given thread is revisited roughly every
  `N × 10 ms`.
- `BLOCKING_POLL_INTERVAL_US = 10_000` (`src/syscall/poll.rs:34`) — the per-loop
  cap on how long `epoll_pwait` blocks before re-polling, even when an
  event-driven waker is registered.

## How `epoll_pwait` actually waits (`src/syscall/poll.rs:394`)

It is a **hybrid poll/wake loop**, not purely event-driven:

1. `smoltcp_net::poll()` — drive the network stack (RX/TX) *inline*.
2. Snapshot the interest list, check each fd's readiness, registering the
   thread's waker for event-driven wakeup.
3. If any fd is ready → return. If `timeout==0` → return. If timed out → return.
4. Otherwise `schedule_blocking(min(deadline, now + 10ms))` and loop.

Each loop is one `iters`. So `epoll_pwait` re-polls **at least every 10 ms** even
when nothing is ready, and the network stack is only advanced while an
epoll-waiting thread is actually running.

## Evidence from trim6 — the smoking gun

Tail of the log, crush PID 131, the network epoll instance (`epfd=9`,
`timeout_ms=-1` = infinite):
```
[epoll] pwait ret pid=131 epfd=9 timeout_ms=-1 nready=1 iters=107 dur_us=4136760 ...
[epoll] pwait ret pid=131 epfd=9 timeout_ms=-1 nready=1 iters=58  dur_us=2287872 ...
```
- `iters=107` over `dur_us=4_136_760` ⇒ **~38 ms per iteration**, despite the
  10 ms cap. The blocked thread is NOT re-dispatched at its 10 ms deadline — it
  waits ~3–4 scheduler quanta. That means ~3–4 other threads sit ahead of it in
  the round-robin each cycle.
- One ready network event therefore took **~4 seconds** to be delivered to the
  goroutine that wanted it. That is the coordination stall, concretely.

Meanwhile `epfd=4` (Go's timer/netpoll instance, short timeouts,
`interest_fds=0`) just spins through `timeout_expired` returns — Go using epoll
as a timer because real wakeups are slow.

## PSTATS shape (crush PID 122, 36 s → 156 s)

```
t=36.67s   futex=440(50673ms)   epoll_pwait=1577(48243ms)   nanosleep=905(29393ms)
t=156.74s  futex=1012(179229ms) epoll_pwait=7008(186008ms)  nanosleep=3755(139114ms)
           sched_yield=202(6123ms)   clock_gettime=79105(345ms)
```
- **`clock_gettime=79105`** in one 30 s window and **`sched_yield` climbing
  27→202** are the classic Go-runtime *busy-spin* signature: `findrunnable` /
  `sysmon` loop on the clock and `osyield` when they can't find work or park
  cleanly.
- **futex wait time balloons (50 s → 179 s) while call count stays modest** —
  goroutines parked on futexes (channel ops, netpoller note) woken late, via the
  10 ms timed fallback rather than a prompt direct wake.
- All of these spinning threads are themselves **RUNNABLE**, so they *increase*
  N in the `N × 10 ms` revisit cost.

## The degradation feedback loop

```
slow epoll/futex wakeup (≥10ms, often ~38ms under load)
        │
        ▼
Go runtime compensates by spinning  ── clock_gettime, sched_yield, 10ms timed futex/nanosleep
        │
        ▼
more RUNNABLE threads in the fixed-slot round-robin
        │
        ▼
longer time to revisit any one thread  (N × 10ms grows)
        │
        └────────────► even slower wakeups  (loop tightens)
```
This is why crush is fine for the first few LLM turns (few goroutines, small N)
and degrades once tool-calling fans out into more concurrent goroutines /
subprocess plumbing (larger N).

## Why tool-calling specifically triggers it

Tool calls fan out concurrency: crush forks+execs subprocesses (`git`, `find`,
`cat`, `jq` — seen in trim6) *and* spins up goroutines to pump their pipes and to
issue follow-up network requests. Each adds runnable threads and more
epoll/pipe fds, pushing N up and tipping the loop above into the visible stall.

## Suggested fixes / probes (in priority order)

1. **Make network-fd epoll truly event-driven.** The waker is already
   registered (`epoll_check_fd_readiness(.., Some(&waker))`); the 10 ms re-poll
   cap should be unnecessary for fds that support wakers. If a socket-ready event
   reliably calls the waker, raise/remove the cap for waker-backed fds so the
   thread sleeps until the real event (deadline only as a backstop). Measure
   wake→dispatch latency first.
2. **Decouple network RX from epoll-thread scheduling.** `smoltcp_net::poll()`
   only runs while an epoll thread runs; under load that starves RX. Drive the
   net poll from the timer IRQ / a dedicated thread so packets are processed
   even when epoll threads are behind.
3. **Prioritise woken threads.** A thread transitioning WAITING→READY via a real
   event should be dispatched ahead of threads that are merely spin-yielding, so
   wakeup latency doesn't scale with N. (Ties to Theory 3.)
4. **Instrument wake→run latency** for one thread to confirm the ~38 ms/iter
   reading and quantify the loop.

## Tests to add (next session)

- **epoll wake latency**: register a waker-backed fd, make it ready from another
  thread, assert `epoll_pwait` returns in ≪ one full `N × 10 ms` cycle (catches
  the "polled, not event-driven" regression).
- **scheduler revisit under load**: with K spin-yielding threads runnable, assert
  a WAITING→READY thread is dispatched within a bounded number of quanta.
- **net RX independence**: assert inbound data is processed without an
  epoll-waiting thread actively running (once fix #2 lands).

---

# RESOLVED — crush goroutine stall was debug-log UART contention (2026-05-27)

**Status: SOLVED for now** (trim9.log). The crush goroutine-coordination stall
documented above was caused by **high-volume debug logging holding IRQs disabled
across synchronous UART writes**, not by a flaw in the futex/epoll/scheduler
primitives themselves. Disabling two hot-path debug flags broke the degradation
loop and crush now runs to completion.

## Root cause

`console::print()` (`src/console.rs:69`) wraps its entire byte-by-byte UART write
loop in `irq::with_irqs_disabled(...)`. The IRQ guard itself is correct (saves and
restores DAIF, `src/irq.rs:16-41`), but every log line means IRQs are masked for
the full duration of writing that line to the PL011 data register. Two aggravating
factors:

1. The UART write does a blind `write_volatile` per byte with **no TXFF (transmit
   FIFO full) check** (`src/console.rs:31-36`; `TXFF` is defined but
   `#[allow(dead_code)]`).
2. Serial is `-serial mon:stdio` (`scripts/run.sh:33`), so output goes to the host
   terminal and QEMU's stdio chardev can apply host-side backpressure on the vCPU
   thread *inside* that MMIO store.

Two chatty flags were **on by default** and fire in exactly crush's hot path, both
routed through `tprint!` → `print()` → IRQs-disabled UART:

- `SYSCALL_DEBUG_NET_ENABLED` (`src/config.rs:282`) — one line per `epoll_pwait`
  return (`src/syscall/poll.rs:100`). crush's netpoller hammers epoll.
- `SYSCALL_DEBUG_PIPE_READ` with `SAMPLE=1` (`src/config.rs:254-258`) — a line on
  **every** pipe read (`src/syscall/fs.rs:73+`). Tool-calling forks subprocesses
  (`git`, `jq`, `cat`) and pumps their pipes.

The ARM generic timer IRQ is level-triggered, so masking doesn't *lose* ticks — but
it *delays* them, and delays VirtIO-net RX servicing too. Under tool-call fan-out
the log volume ramps exactly when concurrency rises, delaying every goroutine
wakeup. This is the concrete driver of the §6 "≈38 ms per epoll iteration despite a
10 ms cap" and the positive-feedback degradation loop.

(Note: `PROC_SYSCALL_LOG_ENABLED`, `src/config.rs:221`, is **not** a UART offender —
it's an in-memory ring buffer via `log::record`, `src/syscall/mod.rs:826`. It only
adds a `uptime_us()` per syscall.)

## Fix applied

Set both hot-path flags to `false` (`src/config.rs:254,282`). Reversible, one line
each.

## Confirmation (trim9.log)

- **crush completes its turn and the box goes idle.** Bad runs (trim3/trim6) showed
  *multiple* PSTATS windows of relentless degradation out to 70–156 s. trim9 shows a
  single PSTATS at 39.66 s, then crush finishes and the system is fully idle through
  209 s (`[Thread0] loop` steady, heap flat ~12.2 MB, `[TMR]` regular).
- **Goroutine coordination is healthy.** SIGURG (sig 23) is delivered to the altstack
  handler and `rt_sigreturn`'d back-to-back ~50 ms apart (`trim9.log:4470-4477`); the
  bad runs took ~4 s on that wakeup path.
- **No feedback loop.** Thread count stayed bounded at 4–7/64, not fanning out into a
  runnable-thread pileup. Throughput rose to 547 syscalls/s vs ~409/s in trim3.
- Interactive: crush crashed a couple of times due to a **corrupted crush DB**
  (unrelated to scheduling); after removing the DB it ran smoothly as expected.

## Structural follow-up (not yet done)

These flags are *diagnostic*; the IRQ-masked-UART coupling is structural, so the
logging is unusable under load even when wanted. The real fix is to make `print()`
not hold IRQs across a synchronous UART loop:

- **Preferred — TX ring buffer + UART TX interrupt.** Writer briefly masks IRQs, pushes
  bytes into a software ring (bounded, tiny critical section), unmasks. The PL011 TX
  IRQ (fires as the FIFO drains / TXFF clears) feeds the hardware from the ring in the
  background. Nobody spins; IRQs are off only for the buffer push, not the device drain.
- **Minimum — honor TXFF, but only with IRQs *enabled*.** Spinning on `TXFF` fixes
  correctness (no dropped bytes on real HW) but NOT latency, and spinning **with IRQs
  masked would reinforce the original bug** — so it must not go in the current
  IRQs-disabled `print()` path as-is.

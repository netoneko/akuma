# `sys_futex` Linux-divergence audit fix: requeue lost-wakeup + four more (2026-08-04)

Closure of the five-op futex audit from
`SELFHOST_DEVBOX_SMOLTCP.md` §"Futex audit: five confirmed Linux
divergences". That audit ran `userspace/forktest/c_stress/futexops.c` (same
stripped static binary) on Akuma **and** on real Linux aarch64 and scored
**5 FAIL on Akuma, 5 PASS on Linux** — every divergence measured, not
inferred. This doc records the code changes that brought Akuma to match Linux
and the A/B that confirmed it.

The current-state description of `sys_futex` lives in
`docs/reference/subsystems/syscalls/sync.md`; this is the history.

## The five divergences and what was done

| # | Divergence (measured on the pre-fix kernel) | Pre-fix site | Fix |
|---|---|---|---|
| 1 | **Requeued waiter was never removed from the requeue target** — the serious lost-wakeup generator | `sync.rs` wait loop (`:266`, `:341-352`) vs `futex_requeue_table` (`:144-168`) | wait loop locates itself across the whole `tgid`; cleanup helper `futex_remove_tid_anywhere` runs on timeout/EINTR |
| 2 | Bitset ignored on `WAIT_BITSET` and `WAKE_BITSET` | `sync.rs:263`, `:386` | queue entries are now `(tid, bitset)`; `futex_do_wake(.., wake_mask)` filters on intersection |
| 3 | Unreadable `timeout` pointer silently meant "no timeout" | `sync.rs:283` | non-null timespec failing `validate_user_ptr`/copy → `EFAULT` after dequeue |
| 4 | `FUTEX_WAKE_OP` never performed the atomic op on `uaddr2` | `sync.rs:447-452` | full read-modify-write of `*uaddr2` per decoded `op` |
| 5 | `FUTEX_WAKE_OP` never did its conditional second wake | same | `(oldval CMP cmparg)` gates a second `futex_do_wake` on `uaddr2` |

### 1. The requeue bookkeeping is now two-sided

`futex_requeue_table` **moves** a waiter's tid from `key1`'s queue into
`key2`'s without waking it — correct. The bug was in the *waiting* thread's
loop: `key` was a local computed once on entry (`sync.rs:266`), and the
post-`schedule_blocking` membership check (`:341-352`) only ever consulted
that one key. After a requeue `key` and the queue the tid actually sits on
disagree, so every loop exit other than "drained by a wake on `key2`"
stranded the tid in `key2`'s queue **permanently**.

Two observable consequences, both reproduced by the probe:

- The requeued waiter's timeout returned **`0` (success), not `ETIMEDOUT`** —
  it found itself absent from `key` and concluded it had been woken,
  legal-ish (spurious wakes are allowed) but it hid the bug.
- The stale tid stayed queued on `key2` forever, and **every stale entry
  silently absorbed one future `FUTEX_WAKE`** on that address: the kernel
  counted it as woken and returned 1, while the thread actually owed that
  wake was never woken. Directly measured: after the requeued waiter timed
  out *and had been `pthread_join`ed*, `FUTEX_WAKE(key2, 1)` still reported
  1 woken. Linux reports 0.

This is the only one of the five on a path ordinary musl programs take
constantly — `pthread_cond_broadcast` *is* `FUTEX_REQUEUE`, and
`pthread_cond_timedwait` supplies the timeout that does the stranding — so it
degrades with process age and was the plausible candidate for the `typenum`
lost-wakeup stall in `SELFHOST_DEVBOX_SMOLTCP.md` "Open issue #2".

**The fix.** On each `schedule_blocking` return the waiter now locates itself
across **all queues in its `tgid`** (requeue never crosses `tgid`, so the
search is bounded to this thread group). Three outcomes drive cleanup:

- *not queued anywhere* → removed by `FUTEX_WAKE` → genuine wake, `return 0`;
- *queued at the original key* → spurious; re-check deadline / pending signal
  / futex value and re-enqueue (the classic futex contract — unchanged from
  before);
- *queued at another key* → requeued; **stay parked** at the requeue target
  (a `FUTEX_WAKE` there will drain us and read as a genuine wake next
  iteration), and on timeout or signal call the new
  `futex_remove_tid_anywhere(tgid, tid)` (`src/syscall/sync.rs:229`) so the
  requeue target is left clean.

Two design points worth pinning, because the obvious simpler fixes are wrong:

- *Why not just have the requeue rewrite the waiter's `key` local?* The waiter
  and the requeue run in different threads with no shared per-thread slot for
  "current futex key" in this kernel. Searching the table on each wake-return
  is the stateless equivalent, and the per-process number of distinct futex
  addresses is small (a handful of condvar/mutex words), so it is cheap in
  practice.
- *Why does the requeued case re-park instead of re-validating the futex
  value?* After a requeue the waiter is committed to the target object, whose
  value contract differs from the original wait's. Re-checking the original
  `val` against `*uaddr2` (or against `*uaddr`) would be a category error; the
  only legitimate exits from the requeue target are a wake there, a deadline,
  or a signal, all of which are now handled. A spurious wake at the requeue
  target just loops and re-parks, still correctly queued.

### 2. Bitset selectivity

The queue changed from `Vec<usize>` to `Vec<(usize, u32)>` (`Waiter = (tid,
bitset)`). Plain `FUTEX_WAIT`/`FUTEX_WAKE` and kernel-internal wakes use
`BITSET_MATCH_ANY` (`0xFFFFFFFF`); `FUTEX_WAIT_BITSET` stores `val3`
(unchanged: `val3 == 0` is still `EINVAL`). `futex_do_wake` gained a
`wake_mask` argument and drains only waiters whose stored bitset intersects
it (`WAKE_BITSET` passes `val3`). Over-waking is usually benign, but a
`WAKE_BITSET` with `val=1` landing on a non-matching waiter would consume the
single wake the matching waiter was owed — exactly the shape the probe
exercised. Rust std always passes `FUTEX_BITSET_MATCH_ANY`, so pure-Rust code
is unaffected; mixed musl/Rust in one process is what this protects.

### 3. Unreadable timeout pointer → `EFAULT`, not infinite park

The pre-fix deadline computation (`sync.rs:283`) was
`if timeout_ptr != 0 && validate_user_ptr(timeout_ptr, 16) { … } else {
u64::MAX }`. So a *non-null but unreadable* timespec — which Linux answers
with `EFAULT` — was silently downgraded to "no timeout". Because
`validate_user_ptr` demand-pages via `ensure_user_pages_mapped`, the
interesting reachability is a demand-page failure under memory pressure:
a transient allocation failure became a permanently parked thread. The WAIT
arm now treats a non-null `timeout_ptr` that fails `validate_user_ptr` *or*
the subsequent user copy as `EFAULT`, dequeueing itself first so no stale
entry is left behind.

### 4–5. `FUTEX_WAKE_OP` fully implemented

The pre-fix stub (`sync.rs:447-452`) "just woke at uaddr" and returned
success, performing neither the mandatory `*uaddr2` write nor the conditional
second wake. The arm now:

1. validates `uaddr2` (non-null, 4-byte aligned, mapped) — `EFAULT` otherwise;
2. decodes `val3` as Linux does — `{shift[31], op[30:28], cmp[27:24],
   oparg[23:12], cmparg[11:0]}`; the `shift` bit turns `oparg` into
   `1 << oparg`;
3. read-modify-writes `*uaddr2` (`SET`/`ADD`/`OR`/`ANDN`/`XOR`; anything else
   `ENOSYS`), capturing `oldval`;
4. wakes up to `val` on `uaddr`;
5. iff `(oldval CMP cmparg)` (`EQ`/`NE`/`LT`/`LE`/`GT`/`GE`, **signed** to
   match Linux), wakes up to `val2` (which rides in the `timeout` argument
   slot — that is why `WAKE_OP` takes no timeout) on `uaddr2`.

musl does not emit `WAKE_OP` (glibc does), so this stays latent for
musl-only userspace but is now correct.

## Verification (2026-08-04, devbox-smoltcp)

Host-built kernel at the fix HEAD, `devbox-smoltcp` feature (`userspace-sshd`
+ `smp-shared`), `release-smp-shared` profile, `DISK=devbox.img MEMORY=4096
SMP=2`. The guest fetched the stripped static `futexops` over
`http://10.0.2.2:8042/futexops` (host `python3 -m http.server 8042`, QEMU
SLIRP host gateway).

`FUTEXOPS` probe, before vs after:

| probe | pre-fix (per the 2026-08-03 audit) | post-fix |
|---|---|---|
| `wake_op_writes_uaddr2` | FAIL | **PASS** — `*uaddr2` updated as Linux specifies |
| `wake_op_second_wake` | FAIL | **PASS** — waiter on `uaddr2` was woken |
| `wake_bitset_selectivity` | FAIL | **PASS** — disjoint bitset woke nobody, rc=0 |
| `bad_timeout_ptr` | FAIL | **PASS** — returned `EFAULT` as Linux does |
| `requeue_timeout_leaves_stale_waiter` | FAIL | **PASS** — no stale waiter left on the requeue target |

Exit code 0, `=== FUTEXOPS DONE — 0 divergence(s) from Linux ===`. The same
binary scores 5 PASS on real Linux aarch64, so the probe set is now
calibrated end-to-end.

Regression check — `userspace/forktest/c_stress/futextest` (pure-C musl
`pthread`, 7 phases) run on the same guest, all `ok`, exit 0:
spawn+join, 200× spawn/join loop, 8-thread fan-out, **mutex+condvar ×2000**
(heaviest `FUTEX_REQUEUE` exerciser — confirms the rewritten wait loop leaves
no stranded tids under churn), 6-thread barrier ×100, wake-before-wait ×500,
park/unpark ×500. The common WAIT/WAKE/condvar/barrier paths are unaffected
by the rewrite.

## What this does and does not close

- **The `typenum` lost-wakeup stall** (`SELFHOST_DEVBOX_SMOLTCP.md`
  "Open issue #2"): divergence #1 is the best-supported lead (the only one of
  the five on musl's `pthread_cond` path, accumulating, "thread parked
  forever on a wake the kernel already counted as delivered"). This fix
  removes the mechanism. It was **not** reproduced under this hypothesis
  before the fix and **not** re-reproduced after; treat it as the fix for the
  most plausible cause, not a confirmed root-cause-and-cure. A `-j4` self-host
  build that survives past the point it previously stalled is the decisive
  re-test.
- **The `-j4` thread-spawn `SIGABRT`** ("current thread handle already set
  during thread spawn"): a different observable (corrupted TLS pointer, not a
  stuck futex) in a different subsystem (the `clone_thread`/TLS path).
  Independent; this change does not touch it. The slot-reclaim fix landed
  earlier in the same archive doc is closer, also not confirmed.

## Unprobed sharp edge (flagged, not fixed)

`futex_key_tgid` → `read_current_pid()` can return a process's own pid in
place of its tgid when the process-table lookup inside it fails
(`crates/akuma-exec/src/process/children.rs:290`,
`...with_process(pid, |p| p.tgid).unwrap_or(pid)`). If that happens the futex
key silently degrades from the **tgid** to the **pid** — waiter and waker
would then key different buckets, the exact stranding shape the requeue fix
above addresses for the in-table case. Not observed; flagged because the
fallback is silent rather than an error.

## Background

Builds directly on:
- `SELFHOST_DEVBOX_SMOLTCP.md` §"Futex audit: five confirmed Linux
  divergences" — the audit, the `[THR-DUMP]` evidence, and what was ruled out
  before the five findings. The pre-fix line numbers and measured behaviours
  cited above are from there.
- `SELFHOST_DEVBOX_SMOLTCP.md` §"Open issue #2" — the
  lost-wakeup stall that prompted the audit.
- `GIT_MISSING_SYSCALLS.md` Issue 14 — the earlier `futex_wake`-only-woke-
  `tgid=0` bug class (musl `pthread_join`), fixed by the tgid-keyed waiter
  table this code already had.
- `AKUMA_SELF_HOSTING.md` §7d — the absolute-vs-relative `FUTEX_WAIT_BITSET`
  timeout bug behind Rust std's timed waits.

The probe (`futexops.c`) and the regression suite (`futextest.c`) live in
`userspace/forktest/c_stress/`; both are static musl aarch64, host-built with
`aarch64-linux-musl-gcc`.

# sync syscalls

`futex` (98) — the only syscall in this family. Source: `src/syscall/sync.rs`.
For the general Waker/wait-queue blocking pattern and the
"never block inside a preemption-disabled closure" rule, see
[`../syscalls.md`](../syscalls.md) "Blocking vs non-blocking" — not
duplicated here.

> **Stability: C (active risk).** Touched repeatedly through Jun 2026
> (`futex fix`, `fixes for futex`, `pthreads fixes in progress` — the last
> as recently as Jun 22); pthread/mutex edge cases are still shaking out.
> The recurring lesson: **`FUTEX_WAIT_PRIVATE` keys on `(tgid, uaddr)`, not
> `(0, uaddr)`** — a wake that only touches the shared (tgid=0) bucket
> silently strands every private-futex waiter (musl's `pthread_join`
> chief among them).

## Waiter table & the tgid key

`FUTEX_WAITERS: Spinlock<BTreeMap<(u32, usize), Vec<usize>>>`
(`src/syscall/sync.rs:12`) keys each wait queue by `(tgid, uaddr)`:

- **`FUTEX_PRIVATE_FLAG` set:** `tgid` = the calling thread's process PID
  (`futex_key_tgid`, via `read_current_pid()`), scoping the futex to one
  process — this is what prevents cross-process VA collisions (no ASLR
  means two unrelated processes can legitimately share a futex address).
- **Not private (shared):** `tgid = 0`.
- **Kernel-internal wakes** (`clear_child_tid`, robust futex): `tgid = 0`
  by convention at the call site, but see `futex_wake` below.

`futex_do_wake(tgid, uaddr, max_wake)` pops up to `max_wake` waiters from
one `(tgid, uaddr)` bucket and fires their wakers. The public
`futex_wake(tgid, uaddr, max_wake)` (used by `clear_child_tid` /
`CLONE_CHILD_CLEARTID` cleanup in `sys_exit`, see [`proc.md`](proc.md))
wakes **both** the shared bucket (`tgid=0`) **and**, if `tgid != 0`, the
private bucket — because the kernel-internal caller cannot know whether the
userspace waiter used `FUTEX_WAIT` or `FUTEX_WAIT_PRIVATE`. This
double-wake is the fix for the bug described in Background below.

## FUTEX_WAIT / FUTEX_WAIT_BITSET

`sys_futex` (`src/syscall/sync.rs:80`), cmd dispatch on `op` (masked of
`FUTEX_PRIVATE_FLAG`/`FUTEX_CLOCK_REALTIME`):

- `uaddr` must be non-null and 4-byte aligned (`EINVAL` otherwise).
- On an unmapped `uaddr`: `FUTEX_WAKE*` returns `0` (no waiters possible)
  and `FUTEX_WAIT*` returns `EAGAIN` rather than `EFAULT` — Go's runtime
  calls `futex(garbage_addr, FUTEX_WAKE)` during exit coordination, and an
  `EFAULT` there breaks the exit path.
- The current value at `uaddr` is read **inside** the `FUTEX_WAITERS` lock,
  atomically with respect to `futex_do_wake` — this closes the classic
  futex lost-wakeup race (value check + enqueue must be one critical
  section).
- **Timeout semantics are op-flag-dependent, not uniform:** plain
  `FUTEX_WAIT` treats the timeout as *relative*; `FUTEX_WAIT_BITSET`
  treats it as *absolute* (`CLOCK_MONOTONIC` by default, `CLOCK_REALTIME`
  if `FUTEX_CLOCK_REALTIME` is set). Rust std's `Condvar`/`Mutex`/`Once`
  timed waits always emit an absolute `FUTEX_WAIT_BITSET` — treating that
  as relative doubled every timed wait's effective duration as uptime grew,
  manifesting as the rustc self-host "futex deadlock" (see
  `archive/AKUMA_SELF_HOSTING.md` §7d).
- The wait loop (`schedule_blocking(deadline)`) distinguishes a genuine
  `FUTEX_WAKE` from a spurious wakeup by re-checking queue membership: if
  `futex_do_wake` already removed this tid, it's genuine (`return 0`);
  otherwise re-check the deadline, a pending signal (`EINTR`), and the
  futex value (`EAGAIN` if changed), then re-enqueue and loop.

## FUTEX_WAKE / FUTEX_REQUEUE / FUTEX_CMP_REQUEUE

- `FUTEX_WAKE`/`FUTEX_WAKE_BITSET`: `futex_do_wake` on the caller's own
  `(tgid, uaddr)` bucket only (unlike the kernel-internal `futex_wake`
  helper, a userspace `FUTEX_WAKE` call always knows which bucket it
  used).
- `FUTEX_REQUEUE`/`FUTEX_CMP_REQUEUE`: wake up to `val` waiters at `uaddr`,
  move up to `val2` (passed in the `timeout_ptr` argument slot — no
  timeout is possible on a requeue op) to `uaddr2`'s bucket.
  `FUTEX_CMP_REQUEUE` additionally checks `*uaddr == val3` before touching
  anything (`EAGAIN` on mismatch).
- `FUTEX_LOCK_PI`/`FUTEX_UNLOCK_PI`/`FUTEX_TRYLOCK_PI`/
  `FUTEX_WAIT_REQUEUE_PI`/`FUTEX_CMP_REQUEUE_PI`: `ENOSYS` (priority-
  inheritance futexes unimplemented).
- Unknown `cmd`: logged and `ENOSYS`, plus a diagnostic dump of the three
  instructions around the trap-frame ELR — a defence against a corrupt
  `op` register (e.g. `-1`) reaching here, which historically indicated
  either register corruption across a context switch or a stale I-cache
  mis-decode (see the `§7k` comment at `src/syscall/sync.rs:404`).

## Known divergences from Linux (measured 2026-08-03)

Five, all **confirmed empirically, not inferred**: `userspace/forktest/c_stress/futexops.c`
probes each one and prints PASS/FAIL. The *same stripped static binary* scores
**5 FAIL on Akuma, 5 PASS on real Linux aarch64**
(`docker run --rm --platform linux/arm64 -v $PWD/futexops:/futexops:ro alpine /futexops`),
so the probes are calibrated, not just assertions about what Linux "should" do.

Ordered by how reachable they are from ordinary musl/Rust userspace:

| # | Divergence | Site | Cost |
|---|---|---|---|
| 1 | **Requeued waiter is never removed from the requeue target** | `sync.rs:134-172` vs `:341-352` | **lost wakeups that accumulate** |
| 2 | Bitset ignored on both `WAIT_BITSET` and `WAKE_BITSET` | `sync.rs:263,386` | a `val=1` wake can be eaten by a non-matching waiter |
| 3 | Unreadable `timeout` pointer silently means "no timeout" | `sync.rs:283` | transient fault → **permanent** park |
| 4 | `FUTEX_WAKE_OP` never performs the atomic op on `uaddr2` | `sync.rs:447-452` | userspace polls a value that can never change |
| 5 | `FUTEX_WAKE_OP` never does its conditional second wake | same | waiters on `uaddr2` park forever |

### 1. The requeue bookkeeping is one-sided (the serious one)

`futex_requeue_table` **moves** a waiter's tid from `key1`'s queue into
`key2`'s queue without waking it (`sync.rs:144-168`) — correct so far. But the
*waiting* thread's loop only ever checks and removes itself from the key it
originally waited on: `key` is a local computed once at `sync.rs:266`, and the
membership check at `:341-352` is `waiters.get(&key)`. After a requeue the two
disagree, and every loop exit other than "drained by a wake on `key2`" leaves
the tid stranded in `key2`'s queue **permanently**.

Two consequences, both observed:

- The requeued waiter's timeout **returns `0` (success), not `ETIMEDOUT`** — it
  finds itself absent from `key` and concludes it was genuinely woken. Legal-ish
  (spurious wakeups are allowed) but it hides the bug.
- The stale tid stays queued on `key2` forever. **Every stale entry silently
  absorbs one future `FUTEX_WAKE` on that address**: the kernel counts it as
  woken and returns 1, while the thread actually owed that wake is never woken.

Measured: after the requeued waiter timed out *and had been `pthread_join`ed*,
`FUTEX_WAKE(key2, 1)` still reported **1 woken**. On Linux: 0.

This is the only divergence of the five that sits on a path ordinary musl
programs take constantly — `pthread_cond_broadcast` is `FUTEX_REQUEUE`, and
`pthread_cond_timedwait` supplies the timeout that triggers the stranding. It
degrades with process age: each broadcast+timeout pair can leave another
wake-eating entry.

### 2-5

`WAIT_BITSET` stores no bitset and `WAKE_BITSET` consults none, so both behave
as plain `WAIT`/`WAKE`. Over-waking is usually benign — but a `WAKE_BITSET`
with `val=1` that lands on a non-matching waiter consumes the wake the matching
waiter was owed. Rust std always passes `FUTEX_BITSET_MATCH_ANY`, so pure-Rust
code is unaffected; mixed musl/Rust in one process is not. (`val3 == 0` is
correctly rejected with `EINVAL`, `sync.rs:274`.)

The timeout-pointer gap is `sync.rs:283`: the deadline is computed only
`if timeout_ptr != 0 && validate_user_ptr(timeout_ptr, 16)`, and the `else` arm
is `u64::MAX`. So a *non-null but unreadable* timespec — which Linux answers
with `EFAULT` — is silently downgraded to an infinite wait. Note
`validate_user_ptr` demand-pages via `ensure_user_pages_mapped`, so this arm is
reachable under memory pressure, where it converts a transient allocation
failure into a thread that never wakes again.

`FUTEX_WAKE_OP` is documented in-source as "For now, just wake at uaddr"
(`sync.rs:447-452`) and returns success. It performs neither the mandatory
`*uaddr2 = (oldval OP oparg)` write nor the conditional second wake. musl does
not emit `WAKE_OP` (glibc does), so this is latent for musl-only userspace.

> **A related sharp edge, not probed:** `futex_key_tgid` → `read_current_pid()`
> ends in `.unwrap_or(pid)` (`process/children.rs:290`). If the process-table
> lookup fails, the futex key silently degrades from the **tgid** to the
> thread's own **pid** — waiter and waker would then key different buckets, the
> exact stranding shape the Stability note above warns about. Not observed;
> flagged because the fallback is silent.

## Background

- `archive/SELFHOST_DEVBOX_SMOLTCP_2026-08-02.md` "Open issue #2" — the
  lost-wakeup stall that prompted the audit above, and what it did and did
  not explain.
- `archive/GIT_MISSING_SYSCALLS.md` Issue 14 — `futex_wake` only woke the
  `tgid=0` queue, missing `FUTEX_WAIT_PRIVATE` waiters (musl's
  `pthread_join`), compounded by `sys_exit` never running the
  `clear_child_tid` wake at all. Root cause + fix for the tgid-keyed
  waiter table above.
- `archive/GOLANG_IPC.md` — `futex_wake` not finding waiters across
  `CLONE_VM` threads (an earlier symptom of the same class of bug).
- `archive/AKUMA_SELF_HOSTING.md` §7d — the absolute-vs-relative timeout
  bug behind Rust std's timed waits.
- `archive/SIGNAL_DELIVERY.md` — `EINTR` vs `SA_RESTART` interaction with
  `FUTEX_WAIT`.

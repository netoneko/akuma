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

## Background

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

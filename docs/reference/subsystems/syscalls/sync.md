# sync syscalls

`futex` (98) — the only syscall in this family. Source: `src/syscall/sync.rs`.
For the general Waker/wait-queue blocking pattern and the
"never block inside a preemption-disabled closure" rule, see
[`../syscalls.md`](../syscalls.md) "Blocking vs non-blocking" — not
duplicated here.

> **Stability: C (active risk).** Touched repeatedly through Jun–Aug 2026
> (`futex fix`, `fixes for futex`, `pthreads fixes in progress` — the last
> as recently as Jun 22); pthread/mutex edge cases are still shaking out.
> The recurring lesson: **`FUTEX_WAIT_PRIVATE` keys on `(tgid, uaddr)`, not
> `(0, uaddr)`** — a wake that only touches the shared (tgid=0) bucket
> silently strands every private-futex waiter (musl's `pthread_join`
> chief among them).

## Wait/wake flow

The designed mechanism, as implemented (`sys_futex`, `futex_do_wake`,
`schedule_blocking`/`publish_waiting_and_take_pending_wake` in
`crates/akuma-exec/src/threading/mod.rs`):

```mermaid
sequenceDiagram
    participant W as Waiter (thread A)
    participant K as sys_futex (kernel)
    participant T as FUTEX_WAITERS table
    participant S as Scheduler
    participant N as Waker (thread B)

    W->>K: futex(uaddr, FUTEX_WAIT[_BITSET], val, timeout)
    K->>T: lock table, read *uaddr
    alt value != val
        K-->>W: EAGAIN (someone already changed it)
    else value == val (read is INSIDE the table lock — atomic wrt futex_do_wake)
        K->>T: enqueue (tid, bitset) at (tgid, uaddr)
        K->>S: schedule_blocking(deadline)
        Note over K,S: publish_waiting_and_take_pending_wake:<br/>store WAITING; check WOKEN_STATES[tid]<br/>— one local-IRQ-masked, SeqCst pair
        S--)W: thread parked (self-SGI, off CPU)
    end

    N->>N: mutate the word at uaddr (e.g. Condvar::notify_all / Mutex unlock)
    N->>K: futex(uaddr, FUTEX_WAKE[_BITSET], count)
    K->>T: lock table, pop up to `count` matching waiters (by bitset)
    K->>S: ThreadWaker::wake(tid) — called OUTSIDE the table lock
    Note over K,S: sets WOKEN_STATES[tid]=true;<br/>if thread state == WAITING, flip READY + ring scheduler
    S--)W: thread resumed

    W->>T: locate self across ALL of tgid's queues (not just the original key —<br/>a FUTEX_REQUEUE may have moved it)
    alt not queued anywhere
        Note over W: popped by FUTEX_WAKE → genuine wake
        K-->>W: return 0
    else still queued at the original key
        Note over W: spurious (revalidation deadline, or a benign race)
        W->>K: re-check deadline / pending signal / value (EAGAIN if changed)
        K->>T: re-enqueue, loop
    else queued at a different key
        Note over W: moved by FUTEX_REQUEUE — stay parked there, no value re-check
    end
```

Two details the diagram compresses that matter for debugging:

- **The value read and the enqueue are one atomic step**, taken under the same
  table lock (`futex_check_and_enqueue`). That's what closes the classic
  futex lost-wakeup race: a concurrent wake either lands before the lock is
  taken (so the read observes the new value → `EAGAIN`, caller re-evaluates)
  or after the enqueue (so it finds the new waiter and wakes it). A wake
  landing in between is impossible by construction — there is no window.
- **The park/wake handshake below `schedule_blocking` is a separate atomic
  pair**, not covered by the table lock above: `publish_waiting_and_take_pending_wake`
  stores `WAITING` and re-checks the sticky `WOKEN_STATES[tid]` flag under one
  local-IRQ mask, because a context switch on this core can only arrive via
  IRQ. Split those two steps and a waker landing in the gap leaves nothing
  behind but a flag nobody reads again. See
  [`scheduler.md`](../scheduler.md) "Park/wake handshake" for the invariant
  and the 2026-08-05 bug this closed.

**Untimed waits get a synthetic deadline.** `FUTEX_WAIT`/`FUTEX_WAIT_BITSET`
with no timeout would otherwise park with `deadline = u64::MAX` — no
schedule-return ever forces a re-check. Since 2026-08-01,
`sys_futex` parks untimed waits with `park_deadline = uptime_us() +
FUTEX_REVALIDATE_US` (200 ms) instead, so the loop above runs periodically
even with no genuine wake: self-remove, re-read the value via
`futex_check_and_enqueue` (which reports `EAGAIN` if it changed — the same
rescue a timed wait gets from its own deadline), re-enqueue, re-park. The
user-visible timeout semantics are unchanged (the `ETIMEDOUT` checks still
compare against the *original* `deadline`, still `u64::MAX` here) — this is
purely a safety net bolted onto the park loop.

> **That safety net has a hole, open as of 2026-08-07.** It can only rescue a
> waiter whose value *did* change but whose wake got lost somewhere in the
> `schedule_blocking`/`ThreadWaker::wake` handshake above (the class the
> 2026-08-05 fix targeted). It cannot rescue a waiter whose value **never
> changes at all** — i.e. thread B in the diagram never reaches "mutate the
> word" / never calls `FUTEX_WAKE`. `[FUTEX-DUMP]`'s `hist=uSepuSepuSep...`
> (unpark→self-remove→re-enqueue→park, repeating, no `W` ever) is what that
> looks like: the revalidation loop runs exactly as designed, forever, because
> there is nothing for it to observe. The open question is what happens
> upstream of thread B's `futex(FUTEX_WAKE)` call — reproduced with a tight,
> ~25s deterministic probe under real SMP contention; not yet root-caused.
> See
> [`../../../runbooks/debug-futex-lost-wakeup.md`](../../../runbooks/debug-futex-lost-wakeup.md)
> "Residual variant" and
> [`../../../archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md`](../../../archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md)
> §7.9–§7.10.

## Waiter table & the tgid key

`FUTEX_WAITERS: Spinlock<BTreeMap<(u32, usize), Vec<(usize, u32)>>>`
(`src/syscall/sync.rs:39`) keys each wait queue by `(tgid, uaddr)`. Each
queued entry is `(tid, bitset)`:

- **`bitset`** is `BITSET_MATCH_ANY` (`0xFFFFFFFF`) for plain `FUTEX_WAIT` /
  `FUTEX_WAKE`, or `val3` for `FUTEX_WAIT_BITSET`. `FUTEX_WAKE_BITSET` only
  drains waiters whose `bitset` intersects its own `val3` (see
  `futex_do_wake`'s `wake_mask` arg).
- **Default (private *or* non-private):** `tgid` = the calling thread's process
  PID (`futex_key_tgid`, via `read_current_pid()`), scoping the futex to one
  address space — this is what prevents cross-process VA collisions (no ASLR
  means two copies of one binary put every global at the same address).
- **`tgid = 0` (the VA-only global namespace):** only when the op is
  non-private **and** `uaddr` falls inside a writable `MAP_SHARED` **file**
  mapping (`mem::is_shared_file_mapping`) — Akuma's entire notion of memory
  genuinely shared between address spaces.
- **Kernel-internal wakes** (`clear_child_tid`, robust futex): published to
  both, see `futex_wake` below.

  > `FUTEX_PRIVATE_FLAG` deliberately does **not** decide this, and Linux does
  > not let it either: `get_futex_key` only reaches the shared `(inode, index)`
  > form for a page with a `page->mapping`, falling back to `(mm, address)` for
  > an anonymous page whichever flag was passed. Keying every non-private op to
  > `(0, uaddr)` (the behaviour before 2026-08-04) put musl's
  > `&__thread_list_lock` — a `libc.bss` global at a fixed VA, waited on with
  > `priv=0` by `__tl_lock` and handed to the kernel as the
  > `CLONE_CHILD_CLEARTID` word by `pthread_create` — in **one queue shared by
  > every musl process on the system**. A `FUTEX_WAKE(addr, 1)` then popped
  > whichever process's waiter happened to be at the head. Deterministic
  > regression probe: `userspace/forktest/c_stress/futexkey.c`; diagnosis:
  > [`../../../runbooks/debug-futex-lost-wakeup.md`](../../../runbooks/debug-futex-lost-wakeup.md).

`futex_do_wake(tgid, uaddr, max_wake, wake_mask)` (`src/syscall/sync.rs:61`)
pops up to `max_wake` waiters whose stored bitset intersects `wake_mask` from
one `(tgid, uaddr)` bucket and fires their wakers. The public
`futex_wake(tgid, uaddr, max_wake)` (used by `clear_child_tid` /
`CLONE_CHILD_CLEARTID` cleanup in `sys_exit`, see [`proc.md`](proc.md)) passes
`BITSET_MATCH_ANY` and wakes **both** the shared bucket (`tgid=0`) **and**, if
`tgid != 0`, the address-space bucket — because the kernel-internal caller
cannot know which namespace the userspace waiter landed in.

This wake is **not** gated on the thread having exited cleanly (`return_to_kernel`
runs it whether or not the thread was already marked TERMINATED). A thread killed
from outside is the case that needs it most: musl passes `&__thread_list_lock` as
the `CLONE_CHILD_CLEARTID` word, so this store+wake *is* how the thread-list lock
is released on thread exit. Skipping it leaves that lock owned by a dead tid and
every later `pthread_create`/`pthread_exit` in the process parks in `__tl_lock`
forever — with a perfectly ordinary, correctly-queued waiter in `[FUTEX-DUMP]`.

## FUTEX_WAIT / FUTEX_WAIT_BITSET

`sys_futex` (`src/syscall/sync.rs:293`), cmd dispatch on `op` (masked of
`FUTEX_PRIVATE_FLAG`/`FUTEX_CLOCK_REALTIME`):

- `uaddr` must be non-null and 4-byte aligned (`EINVAL` otherwise).
- On an unmapped `uaddr`: `FUTEX_WAKE*` returns `0` (no waiters possible)
  and `FUTEX_WAIT*` returns `EAGAIN` rather than `EFAULT` — Go's runtime
  calls `futex(garbage_addr, FUTEX_WAKE)` during exit coordination, and an
  `EFAULT` there breaks the exit path.
- `FUTEX_WAIT_BITSET` with `val3 == 0` is `EINVAL` per spec.
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
- **A non-null `timeout` pointer that is unreadable** (fails
  `validate_user_ptr`, or the subsequent user copy) returns `EFAULT` after
  dequeueing the waiter — not silently downgraded to "no timeout". Note
  `validate_user_ptr` demand-pages via `ensure_user_pages_mapped`, so the
  reachability of this branch is a demand-page failure under memory
  pressure; reporting it beats parking the thread forever.
- **The wait loop is requeue-aware.** On each `schedule_blocking` return the
  waiter locates itself across **all queues in its `tgid`** (requeue never
  crosses `tgid`), not just the key it originally waited on, and branches:
  - *not queued anywhere* → removed by `FUTEX_WAKE` → genuine wake (`return 0`);
  - *queued at the original key* → spurious; re-check deadline (`ETIMEDOUT`),
    pending signal (`EINTR`), and the futex value (`EAGAIN` if changed),
    then re-enqueue and loop;
  - *queued at another key* → moved by `FUTEX_REQUEUE`; stay parked at the
    requeue target (a `FUTEX_WAKE` there will drain us and read as genuine
    next iteration), leaving cleanly on timeout/signal via
    `futex_remove_tid_anywhere` (`src/syscall/sync.rs:229`) so no dead tid
    is left behind to absorb a future wake.

## FUTEX_WAKE / FUTEX_REQUEUE / FUTEX_CMP_REQUEUE / FUTEX_WAKE_OP

- `FUTEX_WAKE`/`FUTEX_WAKE_BITSET`: `futex_do_wake` on the caller's own
  `(tgid, uaddr)` bucket only (unlike the kernel-internal `futex_wake`
  helper, a userspace `FUTEX_WAKE` call always knows which bucket it
  used). `WAKE_BITSET` passes `val3` as `wake_mask`; plain `WAKE` passes
  `BITSET_MATCH_ANY`.
- `FUTEX_REQUEUE`/`FUTEX_CMP_REQUEUE`: wake up to `val` waiters at `uaddr`,
  move up to `val2` (passed in the `timeout_ptr` argument slot — no
  timeout is possible on a requeue op) to `uaddr2`'s bucket, preserving
  each moved waiter's bitset. `FUTEX_CMP_REQUEUE` additionally checks
  `*uaddr == val3` before touching anything (`EAGAIN` on mismatch).
- `FUTEX_WAKE_OP`: performs the mandatory atomic op on `uaddr2`
  (`*uaddr2 = oldval OP oparg`, with `OP`/`oparg`/`CMP`/`cmparg` decoded
  from `val3` as `{shift[31], op[30:28], cmp[27:24], oparg[23:12],
  cmparg[11:0]}`; the `shift` bit turns `oparg` into `1 << oparg`), wakes
  up to `val` waiters on `uaddr`, then — iff `(oldval CMP cmparg)` (signed
  comparison, matching Linux) — wakes up to `val2` (the `timeout` slot) on
  `uaddr2`. `val2` riding in the `timeout` argument slot is why this op
  takes no timeout. musl does not emit `WAKE_OP` (glibc does), so this is
  latent for musl-only userspace.
- `FUTEX_LOCK_PI`/`FUTEX_UNLOCK_PI`/`FUTEX_TRYLOCK_PI`/
  `FUTEX_WAIT_REQUEUE_PI`/`FUTEX_CMP_REQUEUE_PI`: `ENOSYS` (priority-
  inheritance futexes unimplemented).
- Unknown `cmd`: logged and `ENOSYS`, plus a diagnostic dump of the three
  instructions around the trap-frame ELR — a defence against a corrupt
  `op` register (e.g. `-1`) reaching here, which historically indicated
  either register corruption across a context switch or a stale I-cache
  mis-decode (see the `§7k` comment at `src/syscall/sync.rs:641`).

## Background

- `archive/FUTEX_REQUEUE_LOST_WAKEUP.md` — the five-op Linux-
  divergence audit of `sys_futex` (requeue stranding, bitset, timeout
  `EFAULT`, `WAKE_OP`) and the rewrites that brought it to match Linux,
  verified with `userspace/forktest/c_stress/futexops.c`.
- `archive/SELFHOST_DEVBOX_SMOLTCP.md` "Open issue #2" — the
  lost-wakeup stall that prompted that audit, and what it did and did not
  explain.
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

# msgqueue syscalls

SysV message queues: `msgget` (186), `msgctl` (187), `msgrcv` (188),
`msgsnd` (189). Source: `crates/akuma-syscalls-glue/src/msgqueue.rs`, state in
`akuma-syscalls-ipc`. Gated by the crates' `sc-sysv-ipc` feature, which both
kernels turn on only through `linux-extra-syscalls` (Tier 1 — pure dead weight
when off, see [`../syscalls.md`](../syscalls.md) "Feature gates").
`/proc/sysvipc/msg` lists the queues on both kernels.

> **Stability: A (stable).** Dormant since Apr 2026 (only a clippy pass
> since); no crisis cohort ever formed around this family. Caveat: no
> userspace binary in this tree currently exercises SysV message queues
> end-to-end (only musl's headers reference `msgget`/`msgsnd`/`msgrcv`) —
> "dormant" here means untouched, not battle-tested under a real workload.

## Per-box key isolation

`MSGQUEUE_TABLE: Spinlock<BTreeMap<(u64, u32), MsgQueue>>`
(`crates/akuma-syscalls-ipc/src/lib.rs`) is keyed by `(box_id, msqid)`, **not** just
`msqid`. SysV keys (the `key_t` passed to `msgget`) are small integers a
process picks (often via `ftok`), visible to any process on the system on
real Linux — scoping the table by `box_id` stops a process in one
container from opening a queue that happens to share a key with another
container's queue. `msqid`s themselves are allocated from a single global
`NEXT_MSQID` atomic (simpler than per-box counters; the 32-bit space is in
no danger of exhaustion).

**A box's queues die with the box.** `sys_kill_box` calls
`cleanup_box_queues(box_id)` after the kill has taken, without waking
anyone: every thread that could be parked on one of those queues was in the
box. This is required, not tidy-up — herd derives a box id from the box's
name, so a box restarted under the same name gets the same id and would
otherwise inherit the previous generation's queues and undelivered messages.
Boot test: `test_kill_box_removes_its_msgqueues`
([`../../../archive/MSGQUEUE_BOX_TEARDOWN.md`](../../../archive/MSGQUEUE_BOX_TEARDOWN.md)).

## msgget

`IPC_PRIVATE` always creates a fresh queue. Otherwise: look up
`(box_id, key)` in the table; if found, return its `msqid` (`EEXIST` if
`IPC_EXCL` was also passed); if not found and `IPC_CREAT` is set, create
one; otherwise `ENOENT`.

## msgctl

- `IPC_RMID`: removes the queue and wakes every thread parked in its
  `recv_pollers`/`send_pollers` sets (they'll re-check and find the queue
  gone).
- `IPC_STAT`/`IPC_SET`: read/write a `struct msqid_ds` at fixed byte
  offsets into a 112-byte buffer (`key` @0, `mode` @20, `msg_cbytes` @72,
  `msg_qnum` @80, `msg_qbytes` @88 — hand-laid-out, not `#[repr(C)]`, so a
  layout change on either side must be mirrored manually).

## msgsnd / msgrcv — blocking via 10ms poll, not a waker wake

Both block with a **10ms retry loop**
(`akuma_exec::threading::schedule_blocking(uptime_us() + 10_000)`) rather
than the pure Waker-driven pattern described in
[`../syscalls.md`](../syscalls.md) "Blocking vs non-blocking" — `msgsnd`
registers itself in `send_pollers` and `msgrcv` in `recv_pollers` for
`epoll`/`poll` integration, and the *other* side's syscall (`msgrcv`
draining a message, `msgsnd` freeing space) does fire those pollers'
wakers directly. But the waiting side's own loop re-polls on a fixed
10ms timer regardless, rather than trusting the wake alone — so a blocked
`msgsnd`/`msgrcv` has up to 10ms of extra latency after the real wakeup
event, unlike pipe/futex which wake immediately.

- `msgsnd`: validates `msgsz <= MSGMAX` (8192) and mtype `> 0` up front
  (`EINVAL` otherwise); blocks (or `EAGAIN` under `IPC_NOWAIT`) if adding
  the message would push `cbytes` over `MSGMNB` (16384).
- `msgrcv`: `msgtyp == 0` takes the oldest message; `msgtyp > 0` takes the
  first exact-type match; `msgtyp < 0` takes the lowest-type message with
  `mtype <= |msgtyp|` (Linux's three-way `msgtyp` contract). A message
  larger than the caller's `msgsz` is `E2BIG` unless `MSG_NOERROR` is set,
  in which case it's truncated in place.

## Background

[`../../../archive/MSGQUEUE_BOX_TEARDOWN.md`](../../../archive/MSGQUEUE_BOX_TEARDOWN.md)
— queues outlived their box. Otherwise the family was implemented
directly against the SysV IPC spec (`docs/archive/SPLIT_SYSCALLS.md` covers
the `src/syscall/` split generally but predates this module). See
[`../syscalls.md`](../syscalls.md) "The `src/syscall/` split" for where
`msgqueue.rs` sits among the other `sc-*`-gated families.

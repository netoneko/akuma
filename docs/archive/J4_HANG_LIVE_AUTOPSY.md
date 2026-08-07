# Live autopsy of the `-j4` self-host hang — full session record

**Date:** 2026-08-07

**Companion doc:** `KTG_STALE_TID_EXIT_STAMP_J4_HANG.md` is the condensed
root-cause writeup. THIS doc is the verbatim session record: the evidence
trail in the order it was found, the diagnostic techniques that worked, the
dead ends that were ruled out, and the exact verification steps. Written for
the next person staring at a "process waits forever" hang.

**Setup:** VM 73076, `release-smp-shared` SMP=4, 14 GB, `disk_selfhost.img`
`snapshot=on`, SSH on host port 2322. Hangd ~33 min earlier during an in-guest
`cargo build --release -p akuma -j4 --offline`; cargo pid 18 printed
`warning: build failed, waiting for other jobs to finish...` and never exited.
The VM was **never rebooted** — everything below was read out of the live
system and its 140 MB serial log.

---

## 1. Triage: what the periodic kernel dumps said

The serial log carries periodic `[THR-DUMP]` / `[PIPE-DUMP]` / `[FUTEX-DUMP]`
blocks (gated by `DEADLOCK_THREAD_DUMP_ENABLED`). At T2070 (33 min after the
freeze):

```
[THR-DUMP]
  tid=11 st=? pid=26  tgid=18  ... tsc=98 a0=0x304654c8   (cargo futures-timer)
  tid=12 st=? pid=18  tgid=18  ... tsc=98 a0=0x315ce970   (cargo main)
  tid=13 st=? pid=34  tgid=18  ... tsc=98 a0=0x304f8858
  tid=27 st=? pid=125 tgid=18  ... tsc=63 a0=0x15          (blocked read(fd 21))
  tid=17 st=? pid=126 tgid=126 ... tsc=98 a0=0x3d90a5e8   (rustc main)
  tid=20 st=? pid=127 tgid=126 ... tsc=98 a0=0x3cda5fc4   (rustc ctrl-c)
  tid=21 st=? pid=128 tgid=126 ... tsc=63 a0=0x9           (blocked read(fd 9))
[PIPE-DUMP] 7 live
  pipe=14  bytes=3 readers=8 writers=8 pollers=0     <-- jobserver: 3 TOKENS UNREAD
  pipe=88  bytes=0 readers=1 writers=1 pollers=1 (tid=27)
  pipe=89  bytes=0 readers=1 writers=1 pollers=1 (tid=27)
  pipe=93  bytes=0 readers=1 writers=0 pollers=1 (tid=21)  <-- writer GONE
  pipe=94  bytes=0 readers=1 writers=1 pollers=1 (tid=21)  <-- writer... where?
[FUTEX-DUMP] 5 keys — every waiter hist=uSepuSep… or puSXEpuSXE…, queued_for 50–140ms
```

Field decoding (worth writing down — it saved hours):

- `tsc` = the thread's **exact** current syscall (98=futex, 63=read);
  `sc` is the *process-level* field and is misleading for multithreaded procs.
- `a0/a1` = the thread's trapped x0/x1 — for `read` that's the fd, for
  `futex` the uaddr.
- `st=?` = WAITING (the match only names READY/RUNNING/TERMINATED/INITIALIZING).
- Futex hist legend (`src/syscall/sync.rs`): `E`=enqueue `e`=re-enqueue
  `S`=self-removed `W`=woken-by-wake `p`=park `u`=unpark `X`=return-to-EL0.

**Reading the histories correctly was the first pivot.** `uSepuSep…` =
park → spurious unpark → self-remove → re-enqueue → park, **all inside the
kernel** — a healthy untimed wait riding out spurious wakes. Cargo main's
`puSXE…` even returns to userspace each cycle (its normal async-runtime tick;
it has a `futures-timer` thread). Conclusion after ~20 minutes: **the futex
layer is exonerated. No lost wake, no orphan.** The hang had to be upstream:
something never delivers the event these loops poll for. And the jobserver
pipe holding 3 unread tokens killed the "token loss" theory on the spot —
tokens were *available*; nothing was consuming them.

## 2. The pivot: guest procfs

Akuma's procfs turned out to have everything needed: `/proc/<pid>/{cmdline,
fd, stat, status, syscalls}`. (`fd` is a directory of `pipe:[N]` symlinks —
`ls`, don't `cat`.)

`/proc/126/fd` (the idle rustc, compiling `quote-1.0.42`'s build script):

```
0 -> /dev/null   1 -> pipe:[88]   2 -> pipe:[89]
3,4,5,6 -> pipe:[14]                              (jobserver, both ends dup'd)
7 -> pipe:[93]   9 -> pipe:[94]
```

So the blocked `read(fd 9)` is on **pipe 94** — `writers=1`, empty. Then the
decisive sweep: `ls /proc/*/fd | grep pipe` across **every** live pid.
**No process in the system holds a write end of pipe 94.** The kernel counts
a writer that no fd table references → the refcount is leaked → the blocked
reader can never see EOF. That is the hang, mechanically.

`/proc/126/syscalls` (a completion-ordered ring: start-timestamp, NR, dur,
result) gave the freeze moment: at t≈87.03s a `clone` returned pid 138; at
t≈87.93s a `ppoll` returned 1, a `read` returned 0 (EOF — pipe 93, whose
writer had closed properly), then **nothing for the following ~1980 seconds**.

## 3. The pipe refcount ledger

`pipe.rs` logs every `create` / `clone_ref` / `close_read` / `close_write`
with post-op counts. Grepping the serial log for `id=93` / `id=94` (the log
has binary interleaving garbage — **use `grep -a`**, and beware `pid=93` mmap
noise; match `"[pipe] "` prefixed lines) produced a complete double-entry
ledger. Pipes 93/94 = the stdout/stderr rustc created for its linker child:

| event | pipe 93 wc | pipe 94 wc | who |
|---|---|---|---|
| create (T87.0) | 1 | 1 | rustc 126 |
| spawn fork + child dup2 | 3 | 3 | gcc 138 spawned |
| child closes originals, parent closes its ends | 1 | 1 | gcc holds fd1/fd2 |
| gcc→collect2 fork, collect2→ld fork | 3 | 3 | 139, 140 inherit |
| collect2 exit (T87.90, code=0) | 2 | 2 | quad: 93,94,14r,14w |
| gcc exit (T87.92, code=0) | 1 | 1 | quad: 93,94,14r,14w |
| ld teardown | **0** | **1 — never closed** | ONE lone close, no quad |

Every clean exit closed a 4-entry quad (93, 94, jobserver-read,
jobserver-write). The last holder — `ld`, pid 140 — produced exactly ONE
close (`close_write id=93 write_count=0`) and stopped. Its pipe-94 ref AND
both jobserver refs leaked. An fd-table sweep aborted after its first pipe
entry.

## 4. Why ld's teardown aborted: it was never supposed to be dying

pid 140 has **no `[PROC-EXIT]`, no self-`[TERM]`, no `[KTG]`** anywhere in the
log. Its last own action is an `mmap` at T87.89 — still linking. Then:

```
46440: [KTG] my_pid=113 my_tgid=113 by_tid=18 code=0 siblings=2 ...      (T85.9)
48896: [KTG-STALE] my_pid=113 sib_pid=117 tid=31 recycled to pid=Some(140) — not terminating
48898: [T87.89] [mmap] pid=140 ...
48905: [TERM] tid=31 pid=Some(140) by_tid=26 state=1 pending_kill=false at=table.rs:199
48911: [T87.90] [PROC-EXIT] pid=139 collect2 code=0
48918: [T87.92] [PROC-EXIT] pid=138 gcc code=0
48929: [pipe] close_write id=93 write_count=0 read_count=1
48931-48934: [Cleanup] Thread {18,26,30,31} recycled after ~25-32ms cooldown
```

Chain, assembled from code + log:

1. A **concurrent** job's group (pids 112/113/116/117 — the `-j4` build ran
   other crates in parallel) exits at T85.9. `kill_thread_group(113)`
   snapshots `siblings = [(116, tid 30), (117, tid 31)]`. Sibling 117 was
   already dead; PHASE 2 deliberately leaves `thread_id` set on dead siblings,
   so the snapshot records a stale tid.
2. During the 2 s kill grace, slot 31 is recycled to freshly spawned ld
   (pid 140). At grace expiry the hard-kill loop checks ownership and
   correctly skips it — that's the `[KTG-STALE]` line.
3. **PHASE 2 has no such guard** (its own tripwire comment even said so). It
   runs `remove_channel(31)` + `set_exited(0)` — onto **ld's** channel, since
   each slot's new owner re-registers the per-tid channel at spawn, and that
   entry is the *same `Arc<ProcessChannel>`* the per-child-pid registry
   serves to `wait4`.
4. collect2's `wait(ld)` sees `has_exited()==true, code 0` — a forged clean
   exit — reaps live ld (`unregister_process` → the `[TERM] by_tid=26` line;
   legitimate *from wait4's view*), and exits "success". gcc follows. rustc's
   ppoll wakes on pipe 93's genuine EOF.
5. ld's thread, marked TERMINATED while running, enters its kernel teardown
   (`return_to_kernel → cleanup_process_fds → close_all`), closes fd 1
   (pipe 93), and is descheduled at the next preemption point. TERMINATED
   threads are never resumed; the slot is recycled 5 log lines later. The
   sweep is abandoned.
6. **Why the leak was permanent:** `close_all()` did snapshot-then-clear — it
   emptied the fd table *first*, then closed from a local Vec. The abandoned
   entries had already left the table, so the later `Drop`-backstop
   (deferred process reclaim) found nothing to close.
7. rustc blocks in `read(fd 9)` on pipe 94 at T87.93. cargo waits on rustc.
   Hang complete — and 33 minutes later every dump still shows the same
   picture, minus any way to recover.

## 5. Dead ends ruled out (so nobody re-chases them)

- **Futex lost wake / orphan** — exonerated by hist decoding (§1); every
  waiter correctly queued, spurious wakes handled.
- **Jobserver token loss as root cause** — tokens sat unread in pipe 14
  (`bytes=3`) the whole time. The leaked jobserver *refs* are a side effect
  of the same aborted sweep, not the cause.
- **Poll missing the EOF edge on pipe 93** — the `writers=0` pipe DID deliver
  its EOF (the ring shows `read → 0` right after ppoll). tid 21's stale
  poller registrations on 93/94 were leftovers, not the defect.
- **The "spinning VM" (71858, 251% CPU for ~50 min)** — four leftover
  `sh -c 'while :; do :; done'` load generators from the previous session's
  probe experiment. Not a kernel bug. Killed.
- **`Exec format error` on build scripts** — stale `target/`, confirmed again;
  not kernel ENOEXEC.

## 6. Fixes (all in `crates/akuma-exec` + tests; left uncommitted)

1. **`process/mod.rs` — `kill_thread_group` PHASE 2**: channel eviction +
   exit stamp now require `table::pid_for_thread(tid)` to be `None` or the
   sibling itself — the same ownership rule the grace-expiry hard kill and the
   PHASE 2 map eviction already used. Skips print a rate-limited
   `[KTG-STALE-CH]` tripwire. (`None` still proceeds: a FREE slot's leftover
   entry is exactly what the eviction exists to collect, and `has_exited()`
   protects an already-recorded real code from clobber.)
2. **`process/mod.rs` — PHASE 1**, both variants (`request_thread_kill` under
   `kernel_smp_shared`, direct `mark_thread_terminated` single-core): same
   ownership guard, so kill flags are not planted on FREE/recycled slots.
3. **`process/fd.rs` — `SharedFdTable::close_all()`**: pops and closes ONE
   entry at a time (`pop_first` under the lock, close outside it) instead of
   snapshot-then-clear. An abandoned sweep now leaves unclosed entries in the
   table where the `Drop` backstop closes them at reclaim — damage bounded to
   the single in-flight entry.

## 7. Regression test + verification

- New boot-suite test `test_ktg_stale_tid_channel_not_stamped`
  (`src/process_tests.rs`): fabricates the exact scenario — leader + dead
  sibling whose recorded tid is mapped (via `register_thread_pid`) to a
  victim pid, plus a legitimately-owned sibling — runs `kill_thread_group`,
  asserts the victim's channel is untouched (not stamped, not evicted) while
  the owned sibling still receives the group exit code (the goroutine-leader
  case the stamp exists for). Fake tids use `MAX_THREADS + n`: state/kill ops
  are bounds-checked no-ops there while the map/channel registries behave for
  real, and `is_thread_terminated(out-of-range) == true` keeps the smp-shared
  grace-wait from stalling.
- `cargo clippy --workspace` clean; 179 host tests in `akuma-exec` pass.
- Boot suite: 240 PASSED including the new test;
  `kill_thread_group_two_phase` and `external_kill_closes_shared_fds`
  (the nearest neighbors) unaffected.
- The single boot-suite failure, `thread_slot_reclaim_on_spawn`
  (`hot_reclaim=45, want 0`), was **A/B-verified pre-existing**: a stashed
  baseline boot fails it identically (`hot_reclaim=48`). Not from this change.

## 8. Residuals / follow-ups

- **TOCTOU everywhere**: every ownership check is check-then-act; a slot can
  recycle between check and action (µs window vs ~10ms recycle cooldown).
  The rigorous fix is a generation-carrying tid/channel registry
  (WakeHandle-style generations already exist for wakers).
- **Abandoned-execution class is broader than fds**: a thread marked
  TERMINATED mid-teardown still abandons whatever it was doing at the next
  preemption. `close_all` is now tolerant; other teardown steps lose at most
  the in-flight entry's side effects.
- **`wait4` trusts `has_exited()` absolutely.** A cheap belt-and-braces
  tripwire: cross-check `thread_for_pid(child)` liveness before reaping, log
  loudly on disagreement.
- **The RELR instruction-abort class is still open** — it is what makes `-j4`
  attempts *fail*; this fix removes the hang that failures could trigger,
  not the failures themselves.
- **Validation still owed**: rebuild `release-smp-shared` with the fix and
  run the in-guest `-j4` build several times for a real before/after rate.
  One green run means little for a race class.

## 9. Technique notes (reusable)

- The serial log under load contains interleaved/binary garbage: always
  `grep -a`, and anchor patterns on the `[pipe] ` / `[TERM] ` prefixes to
  dodge `pid=93`-style false hits from mmap spam.
- `/proc/<pid>/syscalls` is a completion-ordered ring of
  `(start_ts, NR, dur, result)` — the out-of-order-looking entry near the end
  is just a long syscall that completed late. It answers "when did this
  process last do anything" in one read.
- A **refcount ledger** (create/clone_ref/close per object id, cross-checked
  against a live `/proc/*/fd` sweep) turns "something leaked" into "THIS
  process's teardown missed THIS close between THESE two log lines". It was
  the single highest-value move of the session.
- `[TERM]` lines carry `by_tid` + a source location — `by_tid != `own thread
  at `table.rs:199` means an external reap, and an external reap of a pid
  with no `[PROC-EXIT]` is a killed-while-alive smoking gun.

## Background

- `docs/archive/KTG_STALE_TID_EXIT_STAMP_J4_HANG.md` — condensed root-cause + fix writeup for this defect
- `docs/archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md` — earlier layers of the same `-j4` campaign (§7.15–7.17)
- `docs/archive/STALE_THREAD_SLOT_KILL.md` — origin of the ownership rule the fixes extend
- `docs/archive/GRACE_EXPIRED_HARD_KILL_ORPHANS.md` — the sibling defect in the same function (grace path)
- `docs/archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md` — deferred reclaim; why the `Drop` backstop exists

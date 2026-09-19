# amd64 has no thread-slot recycler — what that costs, and what a fix has to get right

**Written:** 2026-09-18, out of the `[SWITCH FREED-CR3]` investigation
(`AKUMA_AMD64_SWITCH_FREED_CR3_UAF.md` §3), which worked *around* this gap
rather than closing it. **Status: open, not started.** Nothing here is a
regression — it is a divergence that has been true since the x86 scheduler was
written, and it is now written down because a second bug has already come out of
it.

Everything below is verified against the tree at `308580a2` plus the UAF fix.
File:line references are to that state; check them before trusting them.

---

## 1. The claim, precisely

**On AArch64 a thread slot is cleaned when the thread dies. On amd64 it is
cleaned when the slot is next claimed — and the `TERMINATED → FREE` transition
never happens at all.** amd64 slots go `TERMINATED → INITIALIZING`, claimed
directly out of the dead state.

The recycler is `cleanup_terminated_internal` (`crates/akuma-threading/src/lib.rs:2239`),
reached through three public wrappers:

| wrapper | line | callers outside tests |
|---|---|---|
| `cleanup_terminated_lockfree` | 2197 | none |
| `cleanup_terminated_force` | 2203 | tests only |
| `reclaim_terminated_slots` | 2229 | `akuma-kernel-glue:2153` (the AArch64 maintenance loop) · `akuma-vfs-glue/fs.rs:74` (the `akuma-locks-rw` backstop) |

Neither of those two callers is on an amd64 path. `akuma-kernel-glue` is the
AArch64 kernel. `akuma-vfs-glue` **is** a dependency of `amd64`
(`amd64/Cargo.toml:303`), but the registrations live in `fs::init()`
(`fs.rs:40`) and **amd64 calls `fs::mark_initialized()` instead**
(`amd64/src/fs.rs:510`; the comments at `:340`, `:348` and `:364` say so in as
many words).

amd64's side is `x86_claim_slot` (`lib.rs:3009`). Its death paths write the
state and nothing else: `x86_finish_current` (`:3139`) and `x86_abandon`
(`:3126`) are each a single `THREAD_STATES[slot].store(TERMINATED)`.

## 2. Line-by-line: what the recycler does, and whether amd64 does it

The recycler's body is an ordered list, and several entries carry a comment
saying what broke when they were missing. This is that list against amd64.

| recycler step | line | amd64 equivalent |
|---|---|---|
| `ON_CPU` gate (core may still be on the stack) | 2262 | **yes** — `x86_claim_slot`'s `reusable` test, `:3013` |
| `drain_in_flight` gate | 2275 | no — but it is belt-and-braces to `ON_CPU`, which amd64 has |
| cooldown (`thread_cleanup_cooldown_us`) | 2280 | **not needed** — the cooldown exists to guarantee the thread has left its stack, which the `ON_CPU` gate establishes directly |
| `TERMINATED → INITIALIZING` CAS | 2299 | **yes**, `:3017` |
| zero `Context` | 2327 | n/a — x86 `Context` is one `rsp` field |
| `PENDING_SIGNALS` / `PENDING_KILL` / `THREAD_INTERRUPTED` | 2340–2346 | **yes**, via `scrub_thread_slot` at `:3036` |
| `preempt::scrub_slot` | 2353 | **yes**, same |
| `CURRENT_TRAP_FRAME` | 2362 | **yes**, same |
| sigaltstack trio | 2364–2366 | **yes**, same |
| stack free (`kernel_profile_extreme`) | 2373 | n/a — amd64 leaks stacks deliberately, bounded by `MAX_TASKS`, and a recycled slot reuses its pair |
| `CLEANUP_CALLBACK` | 2411 | **no** — see §3.1 |
| `SLOT_PURGE_CALLBACK` (futex) | 2419 | **yes but late** — see §3.2 |
| `bkl::clear_dropped_windows_for_dead_thread` | 2429 | **NO** — see §3.3 |
| `scrub_thread_slot` (catch-all) | 2435 | **yes**, `:3036` |
| `SLOT_REAP_CALLBACK` (dead-holder lock sweep) | 2440 | **NO** — see §3.4 |
| store `FREE` | 2445 | never happens |

So the per-slot *arrays* are handled. **What is missing is every hook that had to
fire at the moment of death, and the one piece of cross-crate state that is
tid-indexed.**

## 3. The four gaps, worst first

### 3.1 `CLEANUP_CALLBACK` never fires — mitigated by hand, but only on one path

`akuma_exec::process::on_thread_cleanup` (`process/mod.rs:436`) removes the
`tid → pid` row and retires the `Process` when its last thread goes. It is
registered by `akuma_exec::process::init()` (`mod.rs:430`), whose only caller is
`akuma-kernel-glue:1109` — so on amd64 it is **neither registered nor
invocable**.

amd64 knows this and hand-rolled the same work in `amd64/src/thread.rs:454`
(`thread_pid_map_remove` + `unregister_process`), with a comment at `:436–448`
that states the divergence exactly and names the symptom of getting it wrong: a
`Process` leaked per `pthread_create`, a `ps` row that never goes away, and
`[TRAMP-MISMATCH]` from `resolve_thread_process`'s table scan finding the dead
`Process` for whoever inherits the task slot.

**The gap is that `thread::teardown` is a path, not a guarantee.** It runs for a
thread that exits normally. A thread killed by a fault, by a group-fatal signal,
or by a deferred kill that never reaches its boundary does not run it — and on
AArch64 the recycler is the backstop that catches exactly those.

**Evidence this is live, not theoretical.** During the clean build on the metal
on 2026-09-18 (the UAF verification run) the console produced:

```
[TRAMP-MISMATCH] tid=7 THREAD_PID_MAP=1174 but table scan found 64 — using 1174
[TRAMP-MISMATCH] tid=7 THREAD_PID_MAP=1177 but table scan found 64 — using 1177
[TRAMP-MISMATCH] tid=7 THREAD_PID_MAP=1178 but table scan found 64 — using 1178
```

three times, same slot, three different pids — i.e. slot 7 being reused while
something stale kept resolving to pid 64. That is precisely the signature
`thread.rs:447` names. **Stated as a strong lead, not a diagnosis**: it has not
been traced to a specific thread that skipped teardown, and that trace is step 1
of the work below.

### 3.2 The futex purge is late

`SLOT_PURGE_CALLBACK` **is** registered on amd64 (`sched.rs:821` →
`futex::purge_task`) and **is** invoked — but from `x86_claim_slot`, i.e. when
the slot is next *claimed*, not when its occupant dies. AArch64 calls it twice:
from `mark_thread_terminated` the instant a kill lands, and again from the
recycler.

So between a thread's death and the next spawn that happens to want its slot, a
dead tid stays queued on its futex key. A `FUTEX_WAKE(uaddr, 1)` in that window
pops the stale entry, counts it toward `max_wake`, and leaves the real waiter
parked — the lost-wakeup shape behind the `-j4` wedge
(`AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §13). The window was *infinite* before
2026-09-18 and is now "until the slot is reused", which on a busy build is short
and on an idle box is not.

### 3.3 BKL dropped-window depth is never cleared — and amd64 opens windows

`bkl::clear_dropped_windows_for_dead_thread` has **exactly one non-test call
site in the tree** (`lib.rs:2429`, inside the recycler). amd64 therefore never
calls it.

That would be harmless if this target never opened a dropped window. It does:
`amd64/src/net.rs:571` and `amd64/src/exec_runtime.rs:138` both
`dropped_window_open()` / `dropped_window_close()`.

The ledger is **tid-indexed**. A thread killed between the open and the close
leaves its depth standing, and the next occupant of that slot inherits it. The
recycler's own comment (`lib.rs:2422–2427`) spells out the consequence on the
other kernel: the new occupant "runs its EL1 excursions BKL-free until the
EL0-entry tripwire healed it". **Whether amd64 has any equivalent healing
tripwire is not established** — that is a question to answer before deciding how
much this one matters, and it is cheap to answer by reading `amd64/src/usermode.rs`'s
syscall entry.

This is the gap most likely to be a real, currently-unexplained bug, because
running BKL-free when you believe you hold the lock is silent and its symptoms
(corruption under SMP load) look like everything else on this box.

### 3.4 Orphaned-lock recovery does not exist on amd64

`SLOT_REAP_CALLBACK` (`ext2::reap_dead_thread`) and
`akuma_locks_rw::register_backstop` are both registered in
`akuma-vfs-glue/fs.rs:72–75` — inside the `init()` amd64 does not call. And the
reap's only invocation is the recycler's `TERMINATED → FREE` transition, which
amd64 does not perform.

So the whole `akuma-locks-rw` recovery design — "release **is** abandon", so a
`panic = "abort"` kill can never wedge a mount permanently
(`AKUMA_EXT2_CLEANUP.md` §4) — is inert here. A thread that dies holding an ext2
lock does not get it released, and the waiter-side backstop that was supposed to
make any single waiter able to drive its own rescue is unregistered.

Check `akuma-locks-rw`'s unregistered behaviour before assuming a hang: the
`Registered` cell carries the message "backstop not registered — call
`register_backstop()` first", and whether that degrades to a plain spin or
panics decides whether this is a latent wedge or merely missing recovery.

## 4. Also worth knowing: `space_root` is amd64's own stale-slot field

Not part of the shared recycler, but the same shape, and it is what made this
document necessary. `amd64::sched::Machine::space_root` is cleared by `finish()`
(`sched.rs`) on the normal exit path only; a thread that dies otherwise keeps it.
With no recycler nothing clears it until `prepare_task_slot` runs at the next
claim.

That was load-bearing for the free gate and is **already handled** — `slot_can_install`
in `any_task_on_space_root` skips TERMINATED/FREE slots that are not `ON_CPU`,
on the proof that such a slot cannot be picked and every route out rewrites the
field first. It is listed here because a recycler that zeroed `space_root` at
death would make that carve-out unnecessary, and because the *next* per-slot
field somebody adds will inherit the same hazard with no carve-out written for
it.

## 5. What a fix has to get right

Four decisions, in the order they matter. This target's constraints are not
AArch64's, so "call `reclaim_terminated_slots` from somewhere" is very likely
the wrong shape.

1. **Who runs it.** The AArch64 answer is the maintenance loop plus a
   waiter-driven backstop, and the backstop exists precisely because "thread 0's
   idle loop does not run while the system is busy" (`BKL_VFS_CARVE_OUT.md`
   §11.4). amd64's idle loop has the same property and the same problem. A
   collector that only runs when the box is idle does not close §3.3 at all,
   because that gap bites *under load*.
2. **Whether it is a collector at all.** The cheaper shape may be to fire the
   missing hooks at the point amd64 *does* know a thread is dead — extend
   `x86_finish_current`/`x86_abandon`, and add the one thing neither covers: a
   path for threads that reach `TERMINATED` without going through either.
   Find that path first; §3.1's `[TRAMP-MISMATCH]` is a live instance of it.
3. **Interaction with `x86_claim_slot`.** Any collector must not race the claim.
   The `TERMINATED → INITIALIZING` CAS is already the arbiter on both sides, so a
   collector that uses the same CAS composes — but note amd64's claim treats
   `TERMINATED && !ON_CPU` as *reusable*, so a collector and a spawn genuinely
   contend for the same slots, where on AArch64 the collector always wins first.
4. **Do not regress the UAF fix.** `slot_can_install`
   (`amd64/src/sched.rs`) assumes every route out of TERMINATED/FREE rewrites
   `space_root` before the slot can run. A recycler that moves slots to `FREE`
   keeps that true (FREE is still covered), but one that *zeroes* `space_root`
   at death makes the carve-out redundant rather than wrong — if you do that,
   simplify the carve-out deliberately and say so, don't leave two mechanisms.

## 6. How to test it on the box

The rig, the install loop and its traps: `docs/runbooks/amd64-bare-metal-loop.md`.
Short version, from inside Akuma:

```sh
kbuild -j 1                                 # incremental; -c for a clean 95-crate build
scripts/install_kernel_amd64.sh             # md5-verified, refuses a headerless ELF
/bin/busybox reboot -f
```

Before the metal, the fast lane costs no reboot:
`python3 scripts/utils/amd64_trials.py --local-only --smp 4` (expect **777
passed, 0 failed** on the current tree; a changed count means you added or
removed a check).

For this bug specifically, the load that exercises dying threads is
`scripts/probes/amd64_kill9_under_load.sh` — `kill -9` against processes blocked
inside a syscall, run with a build going. Threads killed there are exactly the
ones that skip `teardown`.

**Three traps that cost time on 2026-09-18 and will cost it again:**

- **`dmesg` greps match their own argv.** `sshd` logs the command it runs
  (`[SSH] Exec: /bin/sh ["-c", …]`) into the ring `dmesg` serves, so
  `dmesg | grep 'TRAMP-MISMATCH'` reports a hit that is your own grep. Bracket
  every pattern: `grep 'TRAMP-MISMATC[H]'`. It is the `pkill -f` self-match trap
  in a new costume, and it produced one false positive already.
- **A box that stops answering ssh under this load is usually starved, not
  dead.** Four cores building plus the kill probe can deny sshd a completed
  handshake for minutes. Settle it with a TCP connect to 2222 (the
  `SSH-2.0-Akuma_0.1` banner proves the kernel is alive) and a ping, before
  concluding anything.
- **`dmesg` cannot show build-era output unless you drain it first** — the boot
  suite fills the 64 KiB ring. `dmesg -c > /dev/null` before the build, or boot
  `skiptests`. Reading it afterwards and seeing nothing proves nothing.

## 7. Background

- [`AKUMA_AMD64_SWITCH_FREED_CR3_UAF.md`](AKUMA_AMD64_SWITCH_FREED_CR3_UAF.md) —
  the investigation this came out of; §3 is the `space_root` half of §4 above,
  and §2 is the other amd64-never-registers-it bug found the same day.
- [`SELFHOST_KERNEL_HEAP_LEAK.md`](SELFHOST_KERNEL_HEAP_LEAK.md) — why
  `current_thread_is_terminated` gates the deferred-frame drain.
- [`AKUMA_EXT2_CLEANUP.md`](AKUMA_EXT2_CLEANUP.md) §4 — the `akuma-locks-rw`
  reap contract that §3.4 says is inert here.
- [`AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md`](AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md)
  §13 — the futex lost-wakeup shape §3.2 keeps a smaller window open for.
- [`AKUMA_AMD64_THREAD_LIFECYCLE.md`](AKUMA_AMD64_THREAD_LIFECYCLE.md) — this
  target's thread lifecycle as it stands.

---

## Appendix (2026-09-19): the userspace half — nothing reaps an orphaned zombie

The body of this document is about **kernel thread slots**. There is a second,
independent leak one layer up, found while cleaning strays off the trashcan, and
it has the same shape: a dead task's record is never released.

**`init=/bin/sshd`, and `sshd` does not `wait()` on orphans.** On Linux a process
whose parent exits is re-parented to pid 1, and pid 1 reaps it. Here pid 1 *is*
the ssh daemon — it exists to accept connections, not to adopt children — so an
orphaned exited process stays a zombie for the life of the boot.

Measured after a session that had left background probes behind:

```
zombies=17 live=3          # the 3 live: pid 1, plus this ssh session's own sh + ps
```

Every one of the 17 had already exited. They cannot be cleared:

```
State:  Z (zombie)
kill -9 104  ->  rc=0, and /proc/104 is still there two seconds later
```

which is correct behaviour and worth saying plainly, because it reads as
"unkillable process" and sends you looking for a stuck task: **a zombie has no
thread to signal.** `kill` succeeds and changes nothing. Only a `wait4` from the
parent — or a reboot — removes the entry.

Why it matters rather than being cosmetic: each zombie holds a process-table
entry, so this is a slow path to `sh: can't fork`, arriving by a different route
than the scheduler-slot ceiling in `AKUMA_AMD64_COW.md` § "The ceiling that was
not memory". Anything that backgrounds work over an ssh session accumulates
them: the wrapper shell is killed at session teardown, the children it spawned
are orphaned, and nothing reaps them. Most of the 17 above were exactly that.

**Not** the `( cmd; cmd ) &` subshell wedge, which an earlier draft of this
appendix blamed: `AKUMA_AMD64_WAIT4_OWNERSHIP.md` records that as **fixed**
(2026-09-08), and these processes had exited normally — a zombie is not a stuck
task. Check `State:` in `/proc/<pid>/status` before reaching for a wedge
explanation; `ps` alone cannot tell the two apart.

Two candidate fixes, and they are not equivalent:

1. **Reap in pid 1.** Whatever runs as init calls `wait4(-1, WNOHANG, …)` in a
   loop — `userspace/sshd`'s accept loop is the obvious place, since it already
   waits for its own session children. Smallest change, and it fixes only the
   `init=/bin/sshd` arrangement.
2. **Re-parent orphans to pid 1 in the kernel**, so the above is *sufficient*
   rather than accidental. Without this, an orphan whose parent died is not
   pid 1's child at all and `wait4(-1)` will not match it — so check what
   `akuma-exec` does with a dying parent's children before assuming (1) works.

Verify with the census above: run something detached, kill its parent, and count
`State: Z` entries in `/proc/*/stat` field 3. A fix means the count returns to
zero on its own.


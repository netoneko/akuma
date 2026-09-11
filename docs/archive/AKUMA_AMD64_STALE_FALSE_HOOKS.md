# amd64: the `|| false` hooks whose reason had expired

**Date:** 2026-09-11, after the session-stdio fold
(`AKUMA_AMD64_SSHD_SESSION_CHANNEL.md`).
**Prompt:** `proposals/NEXT_AGENT_AMD64_STALE_FALSE_HOOKS.md`.
**Shape:** five constant-answering hook rows replaced by the real table, one
socket-wait hook wired, and one `safe_print!` added so the open `^C`-on-metal
question would be decided by a measurement rather than a theory. It was — and
the answer was somewhere else entirely (§5): `nanosleep` on this target was
uninterruptible, on **both** machines, and a 6x-fast emulated clock had been
hiding that on the one where `^C` looked fine.

## 1. What was wrong

`amd64/src/sched.rs` wrote out `akuma_threading::ProcessHooks` **by hand**,
because this target deliberately does not call `akuma_exec::init`
(`amd64/src/exec_runtime.rs`, "Why this is not `akuma_exec::init`" — that
function registers seven upward surfaces at once, in an order `boot.rs` has not
reached). Five of the table's eight rows answered a constant:

```rust
clear_draining:         |_| {},
drain_in_flight:        |_| false,
pid_for_thread:         |_| None,
find_pid_by_thread:     |_| None,
is_current_interrupted: || false,
```

under two comments, each stating a reason that had since expired:

* *"this target has no signal delivery at all, so a parked thread is never
  interrupted out of its wait. **When signals land here, this is the line that
  changes**"* — signals landed the same day (`AKUMA_AMD64_SIGNAL_DELIVERY.md`,
  `AKUMA_AMD64_FAULT_SIGNALS.md`). The line did not change.
* *"No retired-process reclaim on this target yet — `akuma-exec`'s process table
  is not built here"* — it has been built here since 5b slice 1, and
  `amd64/src` calls `akuma_exec::process::reclaim::drain_retired` from **seven**
  places (`fd.rs`, `usermode.rs` ×3, `sched.rs`, `mem.rs`, `thread.rs`).

That is the interesting property of this defect class and the reason this doc
exists: **a stub with a reason attached reads as a decision.** Every reviewer
since — including the two that wrote the comments — read the sentence, agreed
with it, and moved on. Nothing in the type system, the test suite or clippy can
tell a constant that is still true from one that stopped being true, because the
thing that changed was somewhere else in the tree.

What each stub actually cost:

| row | reader | consequence of `false`/`None` |
|---|---|---|
| `is_current_interrupted` | `akuma_threading::schedule_blocking`, the **x86 park loop** | a thread parked in this target's scheduler is never interrupted out of its wait |
| `drain_in_flight` | the reaper, before freeing a terminated thread's stack | the reaper may free a stack out from under a live `drain_retired` sweep |
| `clear_draining` | `scrub_thread_slot`, on slot recycle | a thread killed inside a sweep leaves `DRAINING[tid]` set; the slot's next occupant takes the "already draining" early return **forever** |
| `find_pid_by_thread` | the `[kill]` cross-thread-kill tracer | the tracer prints nothing on this target — a hang with no attribution |
| `pid_for_thread` | the `[TERM]` lifecycle trace | same, when tracing is on |

The last two rows (`proc_dump_info`, `dump_orphan_processes`) feed
`dump_thread_resume_points`, which **is** a stub on x86_64
(`akuma-threading`, `#[cfg(target_arch = "x86_64")]` → `"[THR-DUMP] not
implemented"`). Those two genuinely have no reader here; they are wired for
uniformity, not effect.

## 2. The fix, and why it is a function rather than wider visibility

Three of the eight fields — `clear_draining`, `drain_in_flight`,
`lifecycle_trace_on` — are `pub(crate)` in `akuma-exec`. From outside the crate
they cannot be named at all, which is precisely why the hand-written copy was
the only option available and why it could only ever answer constants for them.

So the table moved into the crate that owns it:

```rust
// crates/akuma-exec/src/lib.rs
pub fn register_process_hooks() { … }   // called by `init`, and by amd64 directly
```

`amd64/src/sched.rs` now calls `akuma_exec::register_process_hooks()`. Both
kernels register **the same eight function pointers from the same line**, so
this table cannot drift again; a future field gets one answer, not two.

Registration stores function pointers and initialises nothing, so it has no
ordering requirement beyond "before the first park" — it stays where the old
literal was, inside `sched::init`, which `boot::early_init` runs fifth.

`akuma-net`'s `NetRuntime` hook is separate and was wired too
(`amd64/src/net.rs`), to `should_interrupt_blocking_syscall` — the same function
`akuma-kernel-glue` gives it on AArch64, and the union of the deferred-kill bit,
the Ctrl-C / `sys_kill` bit and the per-thread `pthread_kill` set.

### The cost objection that had also expired

`net.rs` said wiring it "is not free: the hook is read from inside `smoltcp`'s
poll loop". That was written when the check was a `get_channel(tid)` `BTreeMap`
lookup behind an IRQ-masked spinlock — the shape that took the bare-metal suite
from 665/0 to 663/3 (`AKUMA_AMD64_SSHD_SESSION_CHANNEL.md` §4). It is now one
relaxed `AtomicBool::swap` on the common path
(`akuma_threading::THREAD_INTERRUPTED`), and `wait_until` asks it **only when
the condition did not hold** (`akuma-net/src/socket.rs:724`, the
`!condition_met &&` short-circuit), so the fast path never reaches it.

That short-circuit is load-bearing for a second reason, and any new caller must
copy it: **the interrupt bit consumes** (`swap(false)`). A speculative caller
eats an `EINTR` that a syscall arm was going to act on.

## 3. The `[ISIG]` trace

`^C` over `ssh` appeared to work on QEMU and not on bare metal (§5 shows that
framing was itself wrong), and the open question was which half of the path was
missing. One rate-limited line answers it, in
`akuma_exec::process::write_to_process_stdin`'s ISIG branch:

```
[ISIG] pid=49 fg_pgid=49 sig=2 members=1
```

`kill_process_group` now returns how many group members it delivered to, and
**zero is the interesting value**: it means the INTR character reached the line
discipline, named a `foreground_pgid`, and found nothing carrying that `pgid` —
a `^C` that raises a signal at nobody, which from the terminal is
indistinguishable from one that never arrived at all. Absence of the line
entirely means the byte never reached the line discipline.

Capped at 8 lines: an interactive session sends one INTR per keystroke, so a
handful is the whole interesting window, and a stuck client resending `0x03`
must not be able to flood a console whose only reader is a person in front of
the machine.

## 4. `scripts/utils/amd64_ctrlc_probe.py`

The `^C` measurement had been run by hand and existed nowhere. It is now a
script, because it has to be repeatable on two machines that are minutes apart.

It times rather than reads: `echo GO; sleep 30; echo NOTREACHED`, wait for `GO`,
send `0x03` after `--delay`, then immediately type `echo BACK`. `BACK` cannot be
echoed until the shell is back in control, so `GO`→`BACK` **is** how long the
foreground job survived — a few seconds if it was killed, the full sleep if it
was not. `NOTREACHED` is reported as corroboration and not relied upon (a torn
`SMP=4` console can swallow it).

A real pty is mandatory and that is why it is `pty.fork()` and not
`subprocess`: `ssh -tt` sends the `pty-req` that sets `SPAWN_FLAG_PTY`, and
without it the session never gets a terminal-backed channel, so the ISIG branch
under test is unreachable by construction.

## 5. What the metal actually said — and the finding it corrects

With the hooks wired, bare metal was re-measured: **still 30.0 s, still
survived.** The park-loop hook was not the cause. The `[ISIG]` line is what
turned that from a dead end into an answer:

```
bare metal:  [ISIG] pid=55 fg_pgid=55 sig=2 members=1
QEMU/TCG:    [ISIG] pid=49 fg_pgid=49 sig=2 members=1
```

Identical. So on the metal the INTR character reaches the line discipline, names
the right `foreground_pgid`, and `kill_process_group` finds exactly one member —
the `sleep`. Delivery is reached. The gap is downstream of it, which rules out
every hypothesis about the tty path, the pty request, the channel and the
process group in one line of output.

Downstream is `amd64/src/usermode.rs`'s `Syscall::Nanosleep`, a busy-yield loop
whose only exit check was `crate::thread::should_leave_now()` — group exit,
nothing else. Its own comment said so: *"this one cannot be interrupted except
by the group exit above"*. So `^C` on a sleeping foreground job was a no-op: the
`SIGINT` stayed pending and was taken at the return to ring 3, i.e. when the
sleep ran out on its own.

### The finding that has to be struck

`AKUMA_AMD64_SSHD_SESSION_CHANNEL.md` recorded `^C` as **working on QEMU (4.8 s)
and broken on bare metal (30.2 s)**, and everything downstream of that — this
doc's own prompt included — treated it as a machine-specific gap. It was not.
`^C` never interrupted a sleeping job on *either* machine.

What differs is the **guest clock**, measured 2026-09-11 by timing a guest
`sleep` from the host:

| | `sleep 5` | `sleep 10` |
|---|---|---|
| bare metal | 5.42 s | 10.49 s |
| QEMU/TCG | **0.90 s** | **1.69 s** |

This target's `uptime_us` is `lapic::ticks() * 10_000` — LAPIC ticks assumed to
be 10 ms apart. Under TCG that counter runs about **six times wall-clock**, so a
30 s sleep finished in ~5 s of real time. The probe sent `^C` at 3 s and saw the
prompt at 4.8 s, which is indistinguishable from an honoured interrupt. The
corroborating observable pointed the same wrong way: `NOTREACHED` was absent on
QEMU, and it was absent because the `sleep` *did* die of the signal — just at the
end of its own (fast) sleep rather than at the keystroke.

Two lessons worth keeping:

* **A timing probe on an emulated machine is measuring two clocks.** The
  elapsed time is the host's; everything the guest does is on the guest's. When
  the two disagree by 6x, a wall-clock threshold tests nothing.
* **"Works on A, broken on B" is a claim about A as much as about B.** Nothing
  in the QEMU run had been checked against "would this also pass if the feature
  did not exist?" — and it would have.

### The fix

`Nanosleep`'s loop now asks
`akuma_exec::process::should_interrupt_blocking_syscall()` — the same check
`akuma_syscalls_time::sys_nanosleep` makes on the other kernel — and returns
`EINTR`. `rem` stays untouched on both paths out, which is what that function
does too; the divergence from Linux (which fills it on an interrupted relative
sleep) is now pinned in one place rather than in each kernel separately.

The check is four relaxed atomic loads on the common path and takes a lock only
once a signal is actually pending, which is what makes it affordable in a loop
that yields at tick rate.

## 6. Measurements

Baselines are the tree of 2026-09-11 before this change; the bare-metal column is
a real A/B — the pre-change kernel was booted back onto the box from
`stage`'s own `.bak` and re-measured.

| gate | before | after |
|---|---|---|
| host unit tests | 1375 / 0 | **1375 / 0** |
| QEMU/TCG amd64 `SMP=4` | 666 / 0 | **666 / 0** |
| amd64 ring-3 check (`-n 20`, `SMP=1`) | OK | **OK** |
| amd64 memory probes (`SMP=1`) | 10 / 10 | **10 / 10** |
| AArch64 `cargo run --release` | 307 / 0 | **307 / 0** |
| Firecracker `SMP=4` | 644 / 0 | **644 / 0** |
| bare metal `SMP=4` | all passed | **666 / 0** |

`^C` over `ssh`, `scripts/utils/amd64_ctrlc_probe.py`:

| | before | after |
|---|---|---|
| QEMU/TCG `SMP=4` | 4.8 s (a fast clock, not an interrupt) | **3.3 s — killed** |
| bare metal `SMP=4` | 30.0 s — survived | **3.3 s — killed** |

3.3 s is the probe's own 3 s delay plus a round trip, so the interrupt is now
immediate on both.

### One thing that is NOT this change

The bare-metal boot carries a `[BKL] stuck … tag=511` storm — **184 lines on the
pre-change kernel, 169 after**. Pre-existing, load-driven, and recorded as such
(`docs/archive/` on tag=511); the A/B is here so the next reader does not spend
an afternoon attributing it to the syscall-prologue change. It also **tears the
self-test tally line** on both kernels, which is why
`Akuma/amd64 - all self-tests passed` is the verdict to grep for on that machine
and the digits are not.

## 7. Background

* `proposals/NEXT_AGENT_AMD64_STALE_FALSE_HOOKS.md` — the prompt this executes.
* `AKUMA_AMD64_SSHD_SESSION_CHANNEL.md` — the session-stdio fold, the
  `THREAD_INTERRUPTED` rewrite, and the `^C` table §5 corrects.
* `AKUMA_AMD64_SIGNAL_DELIVERY.md`, `AKUMA_AMD64_FAULT_SIGNALS.md` — the
  capability that made the first stale comment stale.
* `CTRL_C_SIGINT_DELIVERY.md` — the ISIG branch and the `pgid` broadcast.
* `AKUMA_SELF_HOSTING_AMD64.md` — the walk; C1 is the live trunk.

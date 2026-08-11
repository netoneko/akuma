# `git clone` over HTTPS: two `ITIMER_REAL` bugs, both fixed 2026-08-11

Picked up from `docs/archive/DEVBOX_ISSUES.md` Issue 1 ("`git clone` over
HTTPS deadlocks — pipe fills, nobody drains it") to verify a shallow clone of
Akuma's own repo, from GitHub, over HTTPS, with real (`apk`) git on a
`devbox-smoltcp` VM. The original Issue 1 symptom — zero CPU, zero progress,
permanent hang — did **not** reproduce. Two different, real bugs did, both in
the kernel's `ITIMER_REAL` (`alarm()`/`setitimer()`) implementation
(`src/syscall/time.rs`). Issue 1 is very likely superseded by whatever fixed
Issues 12–14 in `docs/archive/GIT_MISSING_SYSCALLS.md`, not by anything here.

## Reproducing

```bash
scripts/build_devbox_smoltcp.sh   # or just boot an existing devbox.img
overlays/devbox/run-smoltcp.sh
ssh -o StrictHostKeyChecking=no -p 2222 root@localhost \
    'git clone --depth 1 https://github.com/netoneko/akuma.git akuma'
```

`devbox.img` built via `overlays/devbox/bootstrap.sh` ships real `git` at
`/usr/bin/git` (apk package, dynamically linked against `libcurl.so.4` +
OpenSSL 3.x) — `/bin/git` stays a symlink to Akuma's own minimal `scratch`
client unless `DEVBOX_GIT=false`. Both bugs below need the real binary; `scratch`
doesn't hit either code path (it isn't multithreaded and doesn't call `alarm()`).

## Bug 1: a stale `ITIMER_REAL` deadline outlives its process, killing the next thing to reuse the slot

**Status: FIXED.**

### Symptom

`git clone https://github.com/...` died **instantly** — well under a second
after `git-remote-https` started — with no network activity at all:

```
$ git clone --depth 1 https://github.com/netoneko/akuma.git akuma
Cloning into 'akuma'...
Alarm clock
```

`GIT_TRACE=1` showed the exact death point: `git-remote-https` had just been
`exec`'d and hadn't even resolved DNS yet.

```
10:41:08.214307 run-command.c:764  trace: start_command: /usr/libexec/git-core/git remote-https origin ...
Alarm clock
```

`"Alarm clock"` is the shell's message for a child killed by `SIGALRM` (14)
with no handler installed — i.e. the kernel delivered a real, immediate
SIGALRM to a process that had only just started and had not yet called
`alarm()`/`setitimer()` itself.

### Root cause

`ITIMER_REAL`'s per-thread-slot deadline and interval
(`ITIMER_DEADLINE`/`ITIMER_INTERVAL`, both `[AtomicU64; MAX_THREADS]`) lived as
statics local to `src/syscall/time.rs`, written only by `sys_setitimer` and
read by `check_itimers()` (called every timer tick from
`kernel_timer::on_timer_interrupt`). Nothing ever cleared them.

Every *other* piece of per-thread-slot state (`PENDING_SIGNALS`,
`THREAD_SIGNAL_MASK`, sigaltstack, …) is owned by
`crates/akuma-exec/src/threading/mod.rs` and reset by a single function,
`scrub_thread_slot`, called on every FREE→INITIALIZING claim — its own doc
comment is explicit: *"Adding per-slot state? Add it here, not to a call
site."* The itimer arrays existed in a different crate entirely and were never
plumbed into that scrub, so they were the one piece of per-slot state that
silently survived a slot's reincarnation.

Sequence that reproduces it:

1. Some earlier process on this SSH session — commonly busybox `wget -T N`,
   which implements its download timeout via a plain `alarm(N)` — armed
   `ITIMER_DEADLINE[tid]` for its own kernel thread slot `tid`, then exited
   *without* calling `alarm(0)` to disarm it (not a bug in `wget`; `_exit()`
   doesn't have to clean this up on real Linux either — the *kernel* owns
   clearing per-process itimer state on exit there).
2. Thread slot `tid` goes back to the FREE pool. `ITIMER_DEADLINE[tid]`
   still holds the old, now long-past deadline.
3. A brand-new, unrelated process — `git-remote-https` — gets `tid` handed to
   it by `claim_free_slot`.
4. The very next timer tick, `check_itimers()` sees
   `ITIMER_DEADLINE[tid] > 0 && now >= ITIMER_DEADLINE[tid]` (trivially true —
   the deadline is already seconds or minutes in the past) and fires SIGALRM
   against the new process, which has no handler for it yet. Default
   disposition for SIGALRM is fatal → instant death.

This is the same class of bug flagged elsewhere in this codebase as a
recurring risk of thread-slot reuse (see `DEVBOX_ISSUES.md`'s own note about
`sc` going stale across slot reuse, and the memory trail of "stale
`thread_id`" bugs in earlier sessions) — just a new instance of it, in a piece
of state nobody had connected to the scrub discipline yet.

### Fix

**Files:** `crates/akuma-exec/src/threading/mod.rs`, `src/syscall/time.rs`

Moved `ITIMER_DEADLINE`/`ITIMER_INTERVAL` out of `src/syscall/time.rs` and
into `crates/akuma-exec/src/threading/mod.rs`, alongside every other per-slot
register, with `get_itimer(tid) -> (deadline, interval)` /
`set_itimer(tid, deadline, interval)` accessors. `scrub_thread_slot` now
clears both to `(0, 0)` on every slot claim, exactly like `PENDING_SIGNALS`
and the sigaltstack fields next to it. `sys_setitimer` and `check_itimers` in
`src/syscall/time.rs` were rewired to call the accessors instead of touching
local statics — no behavior change for a live process armed itimer, only for
the stale-slot case.

### Test

`crates/akuma-exec/src/threading/mod.rs`, `itimer_tests` module (slots
20–24, disjoint from every other test module's fixed ranges in that file):

- `itimer_state_is_independent_per_thread` / `itimer_out_of_range_tid_is_noop`
  — basic accessor sanity.
- `scrub_thread_slot_clears_stale_itimer_on_slot_reuse` — the actual
  regression: arms an itimer on a claimed slot, releases it, re-claims the
  *same* slot (a narrow 2-wide range pins this), and asserts the deadline
  came back `(0, 0)`. Confirmed this fails (`left: (1, 0), right: (0, 0)`)
  against the pre-fix `scrub_thread_slot` and passes after.

```bash
cargo test -p akuma-exec --target $(rustc -vV | grep '^host:' | cut -d' ' -f2) itimer_tests
```

## Bug 2: `check_itimers` force-interrupts a blocking syscall even for an `SA_RESTART` handler

**Status: FIXED.**

### Symptom

With Bug 1 fixed, `git clone` of a small repo (e.g. `octocat/Hello-World`,
29 files) worked every time. Cloning **Akuma's own repo** (856 files) over
the *interactive* SSH exec channel (not redirected to a local file) failed
**intermittently** — roughly 30–40% of runs — partway through the "Updating
files" (checkout) phase:

```
$ git clone --depth 1 https://github.com/netoneko/akuma.git akuma
Cloning into 'akuma'...
$ echo $?
130
```

`130` is the shell's `128 + signal` convention, so this reads as "killed by
SIGINT (2)" — but it wasn't. `--no-progress --quiet` made the clone succeed
100% of the time (15/15 in one run), which was the first real clue: the
progress meter's output stream, not the checkout logic itself, was involved.

### Ruling out an actual signal

The kernel has three **unconditional** (no debug flag needed) print sites
that would fire for any of the ways a real signal could kill a process:

- `apply_default_signal_action` (`src/exceptions.rs`): `"[signal] Process …
  terminated by signal N (default action)"` — for a real fatal SIG_DFL
  signal.
- `try_deliver_signal` (`src/exceptions.rs`): `"[signal] deliver sig=N …"` —
  for a real signal reaching a registered handler.
- `kill_thread_group` (`crates/akuma-exec/src/process/mod.rs`): `"[KTG]
  my_pid=… code=… …"` — for *any* thread-group teardown, whatever the cause.

None of the first two ever appeared for any `git`-family pid across dozens of
repro runs. The third *did* appear — `[KTG] my_pid=87 my_tgid=87 by_tid=15
code=130 …` — but `my_pid == the process itself`, and `sys_exit_group`
(`src/syscall/proc.rs`) is the only caller that invokes `kill_thread_group`
with `my_pid` equal to the caller's own pid. That means `git` called
`exit_group(130)` **on itself**, from ordinary userspace code — not something
the kernel signaled it with. (`git`'s own `run-command.c` uses exactly this
`exit(128 + sig)` convention when *it* believes a child died by a signal —
the actual death was one level removed from what the exit code implies.)

`/proc/<pid>/syscalls` (`PROC_SYSCALL_LOG_ENABLED`, on by default — see
`src/syscall/log.rs`) confirmed `git`'s last several dozen syscalls before
death were an ordinary `openat`/`fstat`/`mmap`/`write`/`munmap` per-file
checkout sequence with no error results visible — consistent with something
external asking the write loop to stop, not a crash mid-write.

### Root cause

`should_interrupt_blocking_syscall()`
(`crates/akuma-exec/src/process/children.rs`) has two independent halves:

1. `is_current_interrupted()` — a blunt, **`SA_RESTART`-blind** flag
   (`ProcessChannel::interrupted`), documented as "set solely by Ctrl-C and
   `sys_kill`".
2. `current_thread_has_pending_interrupt()` — checks `PENDING_SIGNALS` for a
   signal with a registered handler that does **not** have `SA_RESTART` set;
   correctly leaves an `SA_RESTART` handler's blocking syscalls alone.

`check_itimers()` (`src/syscall/time.rs`) fires **both** paths on every
expired itimer, unconditionally: `interrupt_thread(tid)` / `ch.set_interrupted()`
(path 1) *and* `pend_signal_for_thread(tid, 14)` (feeds path 2). Path 1's own
doc comment explains why it exists: `current_thread_has_pending_interrupt`
"by design only reports signals with a registered handler" and so can never
break a handler-less `alarm(); pause();` out of its block — path 1 is there
specifically to cover that case.

But path 1 doesn't check disposition *at all* before firing — so it also
fires for a process that installed a `SIGALRM` handler *with* `SA_RESTART`,
i.e. a periodic heartbeat/low-speed-limit style timer that explicitly asked
the kernel not to interrupt its blocking syscalls. `git`'s binary links
`alarm`/`setitimer` symbols (confirmed via `strings /usr/bin/git`), consistent
with libcurl's low-speed-limit or similar periodic-alarm mechanism running
during the checkout phase. Every time that alarm ticked, `check_itimers`
force-interrupted `git`'s in-progress blocking `write()` to the SSH
exec-channel pipe (`ProcessChannel::write_bounded`,
`crates/akuma-exec/src/process/channel.rs`) via the `SA_RESTART`-blind flag —
*regardless* of `SA_RESTART` — causing `EINTR`, which apparently isn't a case
`git`'s progress-meter write path retries cleanly, and which it (incorrectly,
from a diagnostic standpoint, but plausibly from git's own perspective)
reported as if a child had died by signal 2.

Streaming through the SSH channel (slow, one network round trip per chunk)
gave this alarm many more real-world ticks to land during than writing to a
fast local file — hence "always reproduces over SSH with progress, never
reproduces redirected to a file or with `--no-progress`."

### Fix

**Files:** `crates/akuma-exec/src/process/types.rs`, `src/syscall/time.rs`

Added `SignalAction::wants_itimer_force_interrupt(&self) -> bool`
(`crates/akuma-exec/src/process/types.rs`, next to `SignalAction`/
`SignalHandler`): `true` for `SIG_DFL` (preserves the handler-less-`pause()`
case), `false` for `SIG_IGN`, and for a registered handler only when
`SA_RESTART` is **not** set. `check_itimers` now calls
`wants_force_interrupt(tid)` (a thin `src/syscall/time.rs` wrapper that
resolves `tid` → its process → its SIGALRM `SignalAction`) and skips
`interrupt_thread`/`ch.set_interrupted()` when it returns `false`.
`pend_signal_for_thread(tid, 14)` still always runs, so an `SA_RESTART`
handler still receives the signal normally at the next syscall return — it
just isn't also force-kicked out of an in-progress blocking syscall.

### Verified with a targeted A/B

Before concluding this was the mechanism, `PTHREAD_KILL_EINTR_ENABLED`
(`src/config.rs`, an existing "clean kill switch" flag gating
`current_thread_has_pending_interrupt`) was flipped to `false` and the whole
kernel rebuilt/rebooted as a control: 10 repeats of the akuma clone still
failed 4/10 times, ruling that path out and pointing squarely at path 1
(`is_current_interrupted`/`check_itimers`). Reverted, applied the real fix
above, rebuilt again: **15/15**, then **10/10** more clean clones (`git
status --short` empty, correct `HEAD`, all 856 files present) on two separate
fresh boots — versus roughly 60% success before.

### Test

`crates/akuma-exec/src/process/types.rs`, `signal_action_tests` module:

- `default_disposition_wants_force_interrupt`
- `ignore_disposition_never_wants_force_interrupt`
- `handler_without_sa_restart_wants_force_interrupt`
- `handler_with_sa_restart_does_not_want_force_interrupt` — the regression:
  confirmed this fails against a stub that always returns `true` (the old
  behavior).
- `sa_restart_bit_ignored_for_non_userfn_handlers`

```bash
cargo test -p akuma-exec --target $(rustc -vV | grep '^host:' | cut -d' ' -f2) signal_action_tests
```

`check_itimers`/`wants_force_interrupt` themselves live in `src/syscall/time.rs`
(the kernel binary, not a host-testable crate — `cargo test -p akuma` has no
library target), so the disposition *decision* was deliberately factored out
into the host-testable `SignalAction` method above; the call site itself was
verified via the QEMU A/B, not a host unit test.

## Verifying `wget -T`'s legitimate use of the force-interrupt path still works

Bug 2's fix is a narrowing, not a removal — the handler-less-`alarm()` case
(path 1's whole reason for existing) still needs to work. Confirmed after the
fix:

```
$ time wget -T 3 -O /dev/null http://10.255.255.1/nonexistent
Connecting to 10.255.255.1 (10.255.255.1:80)
wget: download timed out
```

Still times out and reports correctly (busybox `wget`'s `-T` is a bare
`alarm()` with no handler — `SIG_DFL`, exactly the case
`wants_itimer_force_interrupt` keeps returning `true` for).

## Summary

| Bug | Symptom | Root cause | Status |
|-----|---------|------------|--------|
| 1 | Instant, unconditional `SIGALRM` death of a brand-new process | `ITIMER_DEADLINE`/`ITIMER_INTERVAL` lived outside `scrub_thread_slot`'s reset discipline — a slot's stale, already-expired itimer fired against its next occupant | Fixed — moved into `akuma-exec::threading`, scrubbed on claim |
| 2 | Intermittent `exit(130)` mid-checkout, only over the SSH exec channel with progress output | `check_itimers`'s Ctrl-C-style force-interrupt flag ignored `SA_RESTART`, breaking a periodic alarm-based heartbeat (likely libcurl's low-speed-limit timer) out of a blocking `write()` it explicitly asked to keep running | Fixed — gated on `SignalAction::wants_itimer_force_interrupt` |

## Background

- `docs/archive/DEVBOX_ISSUES.md` Issue 1 — the original report this session
  picked up. Its own symptom (permanent hang, zero CPU, missing sideband
  thread) never reproduced here; superseded by these two, unrelated bugs.
- `docs/archive/GIT_MISSING_SYSCALLS.md` — Issues 9, 11–14: the
  `CLONE_THREAD`/sideband-demux-thread/`wait4` history Issue 1 suspected was
  the same shape of bug. Not implicated this time.
- `src/syscall/time.rs` — `ITIMER_REAL` support: `sys_setitimer`,
  `check_itimers`.
- `crates/akuma-exec/src/threading/mod.rs` — `scrub_thread_slot`, the
  single per-slot-state reset point; `itimer_tests`.
- `crates/akuma-exec/src/process/types.rs` — `SignalAction`,
  `wants_itimer_force_interrupt`, `signal_action_tests`.
- `crates/akuma-exec/src/process/children.rs` —
  `should_interrupt_blocking_syscall`, `current_thread_has_pending_interrupt`
  (the `SA_RESTART`-aware half this bug's fix now stays consistent with).

# Devbox-smoltcp issues log

Running log of issues found while dogfooding the devbox-smoltcp image
(`overlays/devbox/`). One entry per issue; each stands alone.

## Issue 1: `git clone` over HTTPS deadlocks — pipe fills, nobody drains it

**Status: Did not reproduce, 2026-08-11 — likely superseded.** Revisited
2026-08-11 to verify a shallow clone of Akuma's own repo over HTTPS with real
git on `devbox-smoltcp`. The exact symptom described below (permanent hang,
zero CPU, missing sideband thread) never reproduced across dozens of clone
attempts. Two different, real bugs did — both in the kernel's `ITIMER_REAL`
implementation, unrelated to the sideband-thread theory below — root-caused,
fixed, tested, and verified (15/15 then 10/10 clean clones):
[`GIT_CLONE_STALE_ITIMER_SIGALRM.md`](GIT_CLONE_STALE_ITIMER_SIGALRM.md).
Leaving the original write-up below for the historical record; it's very
likely this was already superseded by whatever fixed Issues 12–14 in
`GIT_MISSING_SYSCALLS.md`, not by the itimer fixes.

Found 2026-08-10 running `git clone
https://github.com/madebyaris/native-cli-ai.git` inside a devbox-smoltcp VM
(real `apk`-installed git 2.54.0, not `scratch`).

### Symptom

`git clone` hangs forever with zero progress output and zero CPU usage. No
error, no timeout — it just never returns.

### Diagnosis

No gdbstub was attached to this VM (booted without `-s -S`), so this was
diagnosed entirely from the kernel's always-on deadlock diagnostics
(`DEADLOCK_THREAD_DUMP_ENABLED`, `[THR-DUMP]`/`[PIPE-DUMP]`/`[PSTATS]`,
`src/config.rs:221`) plus `/proc` inspection over SSH — no gdb needed.

Process tree:

```
pid 29  /usr/bin/git                         "git clone <url>"
  └─ pid 30  /usr/bin/git                    "git remote-https origin <url>"
       └─ pid 31  /usr/libexec/git-core/git-remote-http  "origin <url>"
```

`[PIPE-DUMP]` showed one pipe completely full:

```
pipe=13 bytes=65536 readers=1 writers=2 pollers=1
  poller tid=13
```

`[THR-DUMP]` showed all three processes parked, none of them reading it:

```
tid=13 pid=31  sc=64  tsc=64   a0=0x1  a1=0x30c910ac   # write(fd=1, ...) — blocked, pipe full
tid=12 pid=30  sc=260 tsc=260  a0=0x1f a1=0x203ffffb24  # wait4(pid=31, ...) — not reading
tid=11 pid=29  sc=93  tsc=260  a0=0x1e a1=0x0           # tsc (authoritative) = wait4(pid=30, ...) — not reading
```

(`sc` is the process-level `current_syscall`, which the code itself documents
as capable of going stale across thread-slot reuse — `crates/akuma-exec/src/threading/mod.rs:944-951`.
`tsc` is the exact per-thread value and is authoritative; pid 29's `sc=93`
disagreeing with its own `tsc=260` is exactly that known staleness, not a
second syscall in flight.)

Two `[THR-DUMP]`/`[PSTATS]` snapshots 30 seconds apart showed **byte-for-byte
identical** syscall counts for all three PIDs (`pid 31`: `write=921`,
`pselect6=56`, `recvfrom=447`, `connect=3`, ... unchanged) — this is a
permanent deadlock, not a slow network.

`/proc/29/fd` confirmed pid 29 holds the *read* end of the stuck pipe at fd 5
(`pipe:[13]`) — it is the intended reader. `/proc/30/fd` showed pid 30 *also*
still has the pipe's write end open at fd 1, alongside pid 31's own copy
(explaining `[PIPE-DUMP]`'s `writers=2`) — a dangling fd pid 30 never closed
after spawning pid 31, though this is secondary to the actual deadlock, not
its cause.

### Root cause (best evidence, not fully nailed down)

`docs/archive/GIT_MISSING_SYSCALLS.md` (Issues 12-14) already documents this
exact shape of problem from an earlier round: git's fetch-pack process is
supposed to run a second, `pthread_create`/`CLONE_THREAD`-spawned "sideband
demux" thread (via `start_async`) that continuously drains the remote
helper's output pipe *while the main thread blocks in `wait4()`* on the
subprocess tree. That's the only way both halves — draining the pipe and
reaping children — happen concurrently in one process.

`/proc/29/status` and `/proc/30/status` both report **`Threads: 1`**. That
demux thread does not exist right now. Either it was never spawned for this
particular negotiation path, or it was spawned and exited/died silently —
this VM had no gdbstub and there's no retroactive syscall-history trace, so
telling those two apart needs a fresh repro with `GDB=1` or temporary
`clone()`/`pthread_create` logging around this code path.

### Reproducing

```bash
scripts/build_devbox_smoltcp.sh
overlays/devbox/run-smoltcp.sh
ssh -o StrictHostKeyChecking=no -p 2222 root@localhost \
    'git clone https://github.com/madebyaris/native-cli-ai.git'
```

Any sufficiently large/slow HTTPS clone should reproduce it — the pipe only
backs up once the remote helper has more buffered output than the (missing)
demux thread would have drained.

### While debugging, unrelated finding: `devbox.img` had broken core symlinks

The `devbox.img` in use when this was found had `ps`, `head`, `tail`, and
other busybox applets returning `not found` despite `/bin/busybox` itself
working — stale/broken symlinks. Root cause not investigated (the fix was
just to rebuild the image via `overlays/devbox/bootstrap.sh`, which recreates
`/bin`'s busybox applet symlinks from scratch — see step 4 of that script).
Worth a closer look if a future devbox.img shows the same symptom, since a
rebuild papering over it once doesn't rule out a `populate_disk.sh` or
`bootstrap.sh` step that produces it non-deterministically.

## Issue 2: Interactive TUI session wedges the whole VM — BKL stuck, core-pinned

**Status: FIXED 2026-08-11.** Root-caused and fixed in the "fix terminal
locks" pass: `sys_poll_input_event`/`sys_read`'s Stdin arm took
`term_state_lock` with preemption disabled but IRQs enabled, and the post-wake
re-acquire could sit in that state long enough under SMP contention for the
watchdog to declare the VM stuck — unbounded regardless of who the holder was.
Deep-dive, the fix, and its verification:
[`TERM_POLL_INPUT_PREEMPTION_FIX.md`](TERM_POLL_INPUT_PREEMPTION_FIX.md)
(§9-§11). One piece is incomplete: the dedicated kernel regression test has
its own bug and is currently disabled (§11) — worth picking up separately.

Found 2026-08-10, same devbox-smoltcp VM as Issue 1, later the same session —
while `meow` was running interactively in TUI mode (idle-polling for keyboard
input, no command in flight). No gdbstub attached this time either.

### Symptom

The VM stops responding entirely — SSH connections hang and time out, the
running interactive `meow` session stops updating. `qemu-system-aarch64`
pegs at ~199% CPU (both `SMP=2` cores spinning). No panic, no crash — the
kernel's own log just keeps printing, forever, with zero forward progress.

### Diagnosis

```
[BKL] stuck: owner=2 waiter=1 tag=511 (aff0+1)
[WATCHDOG] Preemption disabled for 1113ms at step 6 tid=11
[WATCHDOG] disabled at src/syscall/term.rs:432
...
[WATCHDOG] Thread 11 preemption disabled 94132ms (critical)
```

`owner=`/`waiter=` in `[BKL] stuck` are **core IDs**, not PIDs or TIDs —
confirmed against the print site (`crates/akuma-exec/src/sync.rs:845`) and
`KernelLock::held_by` (`sync.rs:819`, `owner == core_id + 1`, matching the
`(aff0+1)` in the log line). So `owner=2 waiter=1` reads as: **core 1 holds
the Big Kernel Lock, core 0 is stuck spinning for it.** (`tag=511` is always
meaningless unless the profiler is on — read `owner=` instead, per
`log_kernel_lock_stuck`'s own comment at `sync.rs:833-835` and the ticket/barge
history in [`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md).)

The watchdog names the stuck thread precisely: `tid=11`, which the
surrounding `[THR-DUMP]`/`[PSTATS]` identify as `pid=15` = `/bin/meow` (the
interactive TUI session itself), `last_core=0` — consistent with `waiter=1`
(core 0). It's stuck inside the kernel's blocking-stdin-read-with-timeout
loop, `src/syscall/term.rs:405-437` — specifically line 432:

```rust
akuma_exec::threading::disable_preemption();
let term_state = term_state_lock.lock();   // <-- watchdog fires here
term_state.input_waker.lock().take();
akuma_exec::threading::enable_preemption();
```

Preemption is disabled, then it tries to acquire `term_state_lock` — and the
watchdog counter climbs past 94 seconds with no progress. That's the shape of
a self-inflicted stall: a thread spinning for a lock *with preemption off* on
one core, while whatever holds the lock (or the BKL blocking this thread's
own forward progress) is on the other core and never gets to run long enough
to release it — each side effectively starves the other.

**Who's actually on core 1 holding the BKL is not proven**, only
circumstantial: the `[THR-DUMP]` snapshot taken just before the stall storm
began shows `tid=9` / `pid=2,tgid=2` (`/bin/sshd`) as the only real
(non-idle, non-watched) process attributed to `last_core=1`, and its
`[PSTATS]` line shows nearly all of its runtime (`477378ms` of `479090ms`
`in_kernel`) spent in `nanosleep` — i.e. a poll loop. That matches
`userspace/sshd/docs/OPTIONAL_PARALLELISM.md`'s description of `sshd` as a
**single-process, single-threaded, cooperative-scheduling** server that polls
across sessions rather than blocking per-session — exactly the kind of
architecture where a long-held lock inside one poll iteration has no sibling
thread to hand off to. That doc is about a *different* problem (core
utilization / fault isolation across SSH sessions, not this wedge), but the
underlying property it documents — sshd never yields to a peer thread because
it doesn't have one — is the same shape of constraint that would make "core 1
is busy and never lets go" plausible without being certain sshd itself is the
`term_state_lock`/BKL holder in this specific stall.

> **That circumstantial link no longer holds as stated (2026-08-10).** `sshd`
> is now process-per-session by default
> (`userspace/sshd/docs/PROCESS_PER_SESSION.md`): sessions are separate
> processes on the real scheduler, so "sshd has no sibling to hand off to" is
> false on any current build. This does **not** mean the issue is fixed — it
> means the reasoning above cannot be used to explain a fresh occurrence, and a
> repro on a current image would be evidence the wedge was never about sshd's
> architecture. Worth re-running before spending time here: the `[PSTATS]`
> `nanosleep`-dominated poll loop that pointed at sshd will now be spread
> across one listener process plus N session processes, so the attribution
> looks different even if the underlying stall is identical. A single point-in-time
snapshot can't prove who held the lock for the whole 94-second window; that
would need the watchdog (or a fresh repro) to also capture the *owner* core's
thread identity at the moment it entered the critical section, not just the
waiter's.

### Not caused by this session's meow/kernel changes

Nothing touched in this session modified `src/syscall/term.rs`,
`crates/akuma-exec/src/sync.rs`, or `/bin/sshd` — every kernel-adjacent file
here was read-only. All source changes this session were confined to
`userspace/meow/src` (tool-call parsing, allocation cleanup, one UI color).
This is a pre-existing kernel bug, first observed under ordinary interactive
use, not a regression from anything fixed alongside it.

### Reproducing

Not yet reliably reproducible on demand — this surfaced during ordinary idle
TUI polling, not a specific command. Best current lead: run `meow` (no `-c`,
i.e. interactive TUI) for an extended idle period on a devbox-smoltcp VM
while `sshd` is otherwise busy (e.g. other SSH sessions cycling), and watch
`[BKL] stuck`/`[WATCHDOG]` output in the kernel log for `disabled at
src/syscall/term.rs:432`.

### Next step, if picked up

Attach gdbstub (`GDB=1`) on a fresh boot and reproduce interactively, or add
temporary logging around `term_state_lock`'s acquire/release to capture which
core (and which thread) actually holds it during a stuck window — the
`[THR-DUMP]` snapshot cadence (every ~30s heartbeat) is too coarse to catch
the moment of entry into the critical section.

## Issue 3: UART console output can interleave across cores — no cross-core lock

**Status: FIXED (shipped, default-on in `release`), 2026-08-11.** Fixed in the
"fix console serialization issue" pass: `console::emit` now takes a
`Spinlock<()>` + owner-core-ID reentrancy guard around the UART write loop
when `kernel_console_lock` is set, closing the cross-core interleave window.
Default ON for the `release` profile (anything with `OPT_LEVEL != "z"`); the
`size`/`extreme-size` profiles are single-core targets where the lock is pure
overhead, so it stays off there unless `CONSOLE_LOCK=1` forces it on. Verified
under `SMP=4` + `cargo build -j4` self-host load. Deep-dive and verification:
[`UART_SMP_INTERLEAVE_FIX.md`](UART_SMP_INTERLEAVE_FIX.md).

Noticed 2026-08-10 while reading `src/console.rs` during the multikernel
removal pass (docs/archive/TRIM_FAT_MULTIKERNEL.md), not from a specific
repro — this was a code-reading finding, not confirmed against a live
garbled-log capture until the fix's own verification pass.

### The race

`console::emit` (the single chokepoint every `print`/`safe_print!`/`tprint!`
call funnels through) does:

```rust
fn emit(bytes: &[u8]) {
    crate::irq::with_irqs_disabled(|| {
        for &b in bytes {
            UART.write(b);
        }
    });
}
```

`with_irqs_disabled` only masks IRQs on the **current core** — it is not a
lock. Under `smp-shared` (real multi-core, the default since 2026-08-10), two
threads on two different cores can both be inside `emit()`'s loop at the same
time, each writing to the same PL011 data register
(`akuma_exec::mmu::DEV_UART_VA`) with nothing serializing the two byte
streams. The result would be byte-level interleaving of otherwise-unrelated
log lines from different cores — exactly the kind of garbled console output
that's easy to misattribute to something else (a formatting bug, a corrupted
string) rather than a genuine missing cross-core lock.

### Why this wasn't caught earlier

Single-core builds (and the old multikernel build, which routed secondary
output through a per-core ring drained serially by the BSP — see the removed
`smp::console_emit`/`console::print_bytes`, docs/archive/TRIM_FAT_MULTIKERNEL.md)
never had two cores hitting the same `UART.write` concurrently. `smp-shared`
becoming the default is what opened this window.

### Next step, if picked up

Wrap the loop body in `emit()` with a small spinlock (cheap: console output
is not a hot path) so the whole per-call byte sequence is atomic across
cores, not just IRQ-safe on one. Confirm with a `SMP=4` boot under concurrent
load (e.g. the fork-hammer / BitTorrent-swarm regimens already used for other
SMP races) and grep the log for any line that doesn't parse as one coherent
message.

## Issue 4: `/proc/cores` is unreadable — `read_at` never forwards it to `read_file`

**Status: OPEN.** Found 2026-08-10 while boot-verifying the multikernel removal
(docs/archive/TRIM_FAT_MULTIKERNEL.md) against an isolated devbox-smoltcp copy.
Pre-existing — confirmed via `git show HEAD:src/vfs/proc.rs` that the bug
predates this session's changes; not a regression from the removal.

### Symptom

```
$ cat /proc/cores
cat: read error: No such file or directory
```

`/proc/boxes` and `/proc/net/*` read fine on the same image; only `/proc/cores`
fails.

### Root cause

`ProcFilesystem::read_at` (`src/vfs/proc.rs`) hardcodes which virtual paths get
forwarded to `read_file` (the function that actually knows how to render
`cores`, `boxes`, `net/tcp`, etc.):

```rust
if path == "boxes" || path.starts_with("net/") || path == "sysvipc/msg" {
    let data = self.read_file(path)?;
    ...
}
```

`"cores"` was never in that list, so a `read()` on the fd falls through to the
rest of `read_at` and ultimately `Err(FsError::NotFound)` — even though
`read_file` has a working `if path == "cores"` branch, and `metadata`/
`list_dir` both know the file exists (`src/vfs/proc.rs:250,694,771`). `open()`
+ `stat()` succeed; only the actual `read()` 404s, matching busybox's "read
error" (as opposed to "can't open") phrasing.

### Next step, if picked up

One-line fix: add `|| path == "cores"` to the `read_at` whitelist above.

More broadly: this is a class of bug — a virtual path known to `read_file`/
`metadata`/`list_dir` but missing from `read_at`'s separate whitelist — that
could recur any time a new `/proc/*` virtual file is added without updating
all four spots in lockstep. Worth a dedicated audit later: enumerate every
path `read_file` (and `metadata`) recognizes and confirm each one is also
reachable through `read_at`, rather than relying on catching each gap by hand
like this one.

## Issue 5: devbox images ship with missing busybox applet symlinks (`wc`, `head`, `ps`, …)

**Status: FIXED.** Both image-build paths now lay down a correct,
relative-target applet symlink set:

- `scripts/populate_disk.sh:252-284` — the essential-symlinks block runs by
  default (whenever `OVERLAY_DIR` is empty) and installs the **full** applet
  set, not just `--full-busybox`. Driven by `busybox --list` so the binary's
  own applet roster is the source of truth.
- `overlays/devbox/bootstrap.sh:128-154` — step 4 does the same for the
  devbox rootfs.

The historical bug (documented in the comment at `populate_disk.sh:257-263`)
was `busybox --install -s` pointing every link at the path busybox was
*invoked* as — `/mnt/disk/bin/busybox`, the mount-container path — which does
not exist in the guest where the image is mounted at `/`, leaving ~295
applet links dangling while the handful written by a bare `ln -sf busybox`
loop kept working. The fix uses relative `ln -sf $BB` targets throughout and
never clobbers a real (non-symlink) binary the image ships
(`git`→scratch, `vi`→neatvi, `tcc`, `meow`, `curl`, …).

**Verified 2026-08-12** on the running `release-smp-shared` devbox image:
`readlink /bin/{wc,head,tail,ps,sleep,sed,awk,grep,ls,cat,sh}` → `busybox`
(relative) for all; zero dangling symlinks under `/bin`; every Issue 5
applet invoked by its `/bin/<applet>` path (`wc -l`, `head -1`, `ps -e`,
`sleep`, `sed`, `awk`, `grep`) returns rc=0 with correct output. Leaving the
original write-up below for the historical record.

**Found** 2026-08-11 during the UART SMP-interleave
verification (`docs/archive/UART_SMP_INTERLEAVE_FIX.md`), which boots
`disk_selfhost.img` and drives an in-VM `cargo build`. Pre-existing —
same symptom as Issue 1's "while debugging" note about `devbox.img`, just
re-confirmed on a different image.

### Symptom

Over SSH on a freshly-built or freshly-refreshed image, basic busybox
applets return "not found" even though `/bin/busybox` itself works:

```
$ wc -l Cargo.toml
/bin/sh: wc: not found
$ ps
/bin/sh: ps: not found
```

`/bin/busybox wc -l Cargo.toml` works as a workaround, so the binary is
there; only the per-applet symlinks in `/bin` are missing.

### Root cause

The image-build paths that lay down `/bin` sometimes skip the busybox
applet symlink step. `scripts/populate_disk.sh` *does* have the logic
(`SYMLINK_CMD`, lines ~230-285, with a "Full applet set by default (not
just `--full-busybox`)" comment explaining why this is the default), and
`overlays/devbox/bootstrap.sh` step 4 lays the same set down for the
devbox rootfs. But neither catches every image-build path:
`disk_selfhost.img` (built per `acceptance/10_selfhost_compile_akuma.md`,
which calls `populate_disk.sh --with-rust-toolchain` and a separate
Docker-based `git clone`) shipped with the applet set missing until a
manual `DISK=disk_selfhost.img scripts/populate_disk.sh --bin-only
--full-busybox` re-pass fixed it mid-investigation.

The Issue 1 note already flagged this same shape against `devbox.img`
and papered it over with a `bootstrap.sh` rebuild. This issue is the same
bug, different image, fix path that doesn't depend on the devbox overlay.

### Next step, if picked up

Two layers, both worth doing:

1. **Make the symlink step idempotent + cheap**, then call it from every
   image-build path that touches `/bin`. The step is already idempotent
   (it skips non-symlinks and uses `ln -sf`); the gap is *invocation*
   coverage, not logic. Specifically:
   - `acceptance/10_selfhost_compile_akuma.md`'s prep sequence (steps
     1-3) should call `populate_disk.sh --bin-only --full-busybox` after
     the toolchain install and source clone, the same way a devbox
     rebuild runs through `bootstrap.sh` step 4.
   - Any new image-build script that writes to `/bin` should be audited
     for the same gap rather than discovering it one "X: not found" at a
     time over SSH.
2. **Add a smoke check** to the boot-time self-test suite (or
     `acceptance/` regimens) that spawns one busybox applet by its
     `/bin/<applet>` path (e.g. `/bin/wc -l /Cargo.toml` from inside the
     VM, or `/bin/wc` via the boot self-tests against a fixture) and
     fails loud if it returns `ENOENT`. That converts "missing symlink"
     from an over-SSH papercut to a CI-visible regression.

## Issue 6: `tail -f` ignores `^C` over SSH — signal doesn't break the blocking read

**Status: OPEN.** Found 2026-08-11 during the same self-host session as
Issue 5 (`docs/archive/UART_SMP_INTERLEAVE_FIX.md`). Not investigated
beyond a positive repro — adding here so it doesn't get lost; would
benefit from a gdb repro before any fix.

### Symptom

Over SSH inside the devbox:

```
$ tail -f /tmp/cargo-build.log
…output…
^C^C^C
…output keeps printing, session does not return to the prompt…
```

`^C` (SIGINT) does not interrupt the running `tail -f`. The only way out
is `kill -9 tail_pid` from a second SSH session, or closing the
connection. Plain `tail` (without `-f`) exits normally on its own and
`^C` works against any other busybox applet tried (`sleep 30`, `wc` over
a pipe) — the bug is specific to the blocking-read loop in `-f` mode.

### Likely shape of the bug

Busybox `tail -f` is a tight `read(fd, buf, sz)` loop on the file's fd
that is supposed to be interrupted by `SIGINT`'s default disposition
(term the process) — but in Akuma that read either:

- isn't returning `EINTR` when SIGINT is delivered mid-syscall (signal
  handler install path setting `SA_RESTART` unconditionally, or the read
  returning to user without checking the signal mask), or
- the signal is being delivered but the busybox loop is structured
  around `read` returning `EINTR` and we're not surfacing it, or
- the signal is being masked/pended indefinitely on this thread and
  never delivered at all.

`docs/archive/GIT_MISSING_SYSCALLS.md` documents signal-delivery bugs of
related shape (Issues 12-14 around `CLONE_THREAD`/`wait4` visibility);
worth checking whether `tail -f`'s read-loop is hitting a related gap
in `pselect6`/`ppoll`/`read` signal-interrupt semantics before assuming
this is its own thing.

### Reproducing

```bash
scripts/build_devbox_smoltcp.sh
overlays/devbox/run-smoltcp.sh
ssh -o StrictHostKeyChecking=no -p 2222 root@localhost \
    'cd /tmp; echo hello > x; tail -f x'   # then mash ^C
```

### Next step, if picked up

Attach `GDB=1` on a fresh boot and reproduce, breakpoint
`sys_read`/`sys_rt_sigaction`/`sys_rt_sigreturn` to confirm which of the
three shapes above is the actual cause. The fix is small once the shape
is known — `EINTR` propagation in `sys_read`, or unsetting `SA_RESTART`
for the busybox install path, or wiring the pending-signal drain on
read-return — but each points at a different file.

## Background

- `docs/archive/GIT_MISSING_SYSCALLS.md` — Issues 11-14, the CLONE_THREAD /
  sideband-demux-thread / `wait4` visibility bugs Issue 1's root cause most
  closely resembles (all previously FIXED; this may be a new gap in the same
  area, not a regression of those specific fixes).
- `docs/archive/GIT_CLONE_STALE_ITIMER_SIGALRM.md` — the two `ITIMER_REAL`
  bugs found and fixed 2026-08-11 while re-verifying Issue 1; a different
  shape of bug than the pipe-deadlock theory above.
- `docs/runbooks/debug-devbox.md`, `docs/runbooks/debug-network.md` —
  general devbox/network triage; `docs/README.md`'s symptom matrix routes
  "`git clone` hangs or wedges" there, with a row added pointing here too.
- `docs/archive/MINIMAL_DEV_BUSYBOX_APPLETS.md` — the curated minimal applet
  set for a stable dev environment, with Tier 1 + Tier 2 verified 2026-08-12
  on a `release-smp-shared` build (49/54 pass; three new bugs found —
  `utimensat` hardcoded to 0, `getgroups` undispatched, missing `/etc/passwd`
  on the devbox overlay). Its procfs/sysfs cluster inventory extends Issue 4's
  "`read_at` whitelist" class of bug to the whole `/proc` + `/sys` surface,
  and Issue 5's "missing applet symlinks" is the image-build side of the same
  "what does a dev expect to find operational" question.
- `userspace/sshd/docs/OPTIONAL_PARALLELISM.md` — Issue 2's circumstantial
  link: sshd's single-process, single-threaded, cooperative-poll-loop
  architecture (not a bug in itself, a documented design tradeoff) as the
  plausible reason core 1 stayed busy long enough to produce a 94-second BKL
  stall, without proof sshd is the specific lock holder in this instance.
- `crates/akuma-exec/src/sync.rs` — `KernelLock`, `log_kernel_lock_stuck`
  (the `[BKL] stuck` print site, confirms `owner=`/`waiter=` are core IDs).
- `src/syscall/term.rs` — the blocking-stdin-read-with-timeout loop where the
  watchdog pinpointed the stall (line 432).
- `src/console.rs` — Issue 3's `emit()` chokepoint (no cross-core lock around
  the UART MMIO write loop).

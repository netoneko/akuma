# Devbox-smoltcp issues log

Running log of issues found while dogfooding the devbox-smoltcp image
(`overlays/devbox/`). One entry per issue; each stands alone.

## Re-test sweep 2026-08-16 — read this before trusting a per-issue Status line

All rows measured the same day on `devbox.img`, `MEMORY=8192 SMP=4`,
`devbox-smoltcp`. **Four of the per-issue Status lines below are now wrong**;
they are left in place (this is an archive log) and corrected here.

| # | Status line says | Measured 2026-08-16 | |
|---|---|---|---|
| 1 | Did not reproduce (08-11) | **not re-tested** — avoided entirely; `devbox.img` ships `/root/akuma` pre-cloned, so nothing needs an in-guest clone | |
| 2 | FIXED | **not re-tested** — reproducing it means wedging the VM | |
| 3 | **FIXED 2026-08-11** | **STILL BROKEN** — see below. ⚠️ | ✗ |
| 4 | OPEN | **confirmed OPEN** — `cat /proc/cores` → `read error: No such file or directory`, while `cores` *is* listed in `/proc` | |
| 5 | FIXED | **confirmed FIXED** — 11/11 applets present (`wc head ps tail sort uniq grep sed awk du df`) | |
| 6 | OPEN | **not re-tested** — needs an interactive `^C` | |
| 7 | OPEN | **premise retired** — `/bin/bash` (GNU bash 5.3.9, a real 866 KB ELF) is installed. Re-test whether the original failure was bash syntax or Issue 8's shebang bug | ✗ |
| 8 | OPEN, "busybox's own trigger" | **ROOT-CAUSED, and misattributed** — a kernel `execve` `argv[0]` bug; the busybox trigger runs clean. See the entry | ✗ |
| 9 | OPEN (no IPv6, cargo can't reach crates.io) | **RESOLVED for cargo** — `cargo search` and `cargo fetch` both work in-guest; a full dep set (14.2 MB) downloaded. Whether IPv6 itself exists was not tested — cargo simply no longer needs it | ✗ |
| 10 | FIXED | **not re-tested** — rump image, different build | |
| 11 | OPEN (three lines every `SMP>1` boot) | **confirmed OPEN, unchanged** — exactly 3 lines at SMP=4 | |
| 12 | OPEN (stdin hangs at 1 MiB) | **confirmed OPEN** — 4 MiB over ssh stdin → `rc=255`, no output. The image-staleness fix in that entry has not been applied to this `devbox.img` | |
| 13 | OPEN (`pwritev2` ENOSYS spam under build load) | **not observed** — a full `-j4` kernel build logged **0** × `nr=287` (only `nr=71` ×6). Either the spam is gone or this workload does not provoke it | ✗ |

### ⚠️ Issue 3 is not fixed — cross-core console interleaving still tears lines

Its Status line reads "FIXED (shipped, default-on in `release`), 2026-08-11".
Measured 2026-08-16 on a `devbox-smoltcp` boot at SMP=4, `[herd] Started sshd
(pid= 2)` came out of the console **split across two lines**, with the prefix
separated from its tail:

```
[herd] Starting service: sshd
sshd (pid= 2)
```

This is not cosmetic. Any harness that waits on a console string can miss a
perfectly healthy boot: it cost a 12-minute self-host trial that scored
`BOOT_FAIL` against a VM where herd was PID 1, sshd was PID 2 accepting at 640
syscalls/s, and a session handler had already forked at PID 3. The repo's own
gate had the same latent false-negative — `scripts/verify_trim.py`'s
`wait_for_marker` matched only `Started sshd|sshd started`, now widened to accept
the surviving tail `sshd (pid=`.

**Rule for anyone writing a harness: never assume a console string arrives
contiguously at `SMP>1`. Gate on an ssh round-trip, which no other core's printf
can tear.** Written up in
[`../runbooks/verify-trim-fat-change.md`](../runbooks/verify-trim-fat-change.md)
§ "Before calling anything a regression" item 4.

Whether the 08-11 fix regressed or only ever narrowed the window is not
established here — this sweep observed the symptom, it did not re-audit the lock.

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

## Issue 7: `userspace/build.sh` needs bash — busybox `sh` can't run it

**Status: OPEN.** Found 2026-08-12 running the in-guest self-host/devbox
userspace build (`docs/runbooks/selfhost-kernel-build.md`): `sh build.sh`
(the guest's `/bin/sh`, busybox ash) fails immediately, and `apk add bash`
was needed just to get the build running at all.

### Symptom

Inside the guest, `/bin/sh build.sh` (or a plain `./build.sh` when `sh` is
the default interpreter) fails at the array assignments near the top of the
script; only `bash build.sh` works.

### Root cause

`userspace/build.sh` uses bash indexed-array syntax (`NAME=(...)` /
`"${NAME[@]}"`) throughout — the rustflags lists (`MEOW_SIZE_FLAGS`,
`EXTERNAL_RUSTFLAGS`, `MEOW_RUSTFLAGS`), the member lists (`EXTERNAL_MEMBERS`,
`NO_BIN_MEMBERS`, `MEMBERS`, `BINARIES`), and several more further down. Arrays
are not part of POSIX `sh` and busybox `ash` doesn't implement them, so the
script hard-requires bash. The guest devbox image doesn't ship bash by
default (it ships busybox `sh`), so building userspace in-guest currently
means `apk add bash` first — an extra ~dependency pull just to run a build
script, on an image that otherwise gets by on busybox applets.

### Reproducing

```bash
overlays/devbox/run-smoltcp.sh   # or run-devbox / self-host disk
ssh -o StrictHostKeyChecking=no -p 2222 root@localhost \
    'cd /root/akuma/userspace && sh build.sh'   # fails: array syntax
```

### Next step, if picked up

Rewrite `build.sh` to avoid bash arrays so it runs under busybox `sh`/dash —
e.g. newline- or space-separated values in a plain variable consumed with a
`for` loop / `set --`, or `printf '%s\n' "$LIST" | while read -r item`. The
harder parts are the rustflags lists, which get joined with `\x1f` via
`encode_rustflags()` (a bash-array-friendly helper) for
`CARGO_ENCODED_RUSTFLAGS` — that join needs to keep working with a
plain-variable/positional-params representation instead of `"${ARR[@]}"`.
Worth auditing the rest of the script for other bashisms (`[[`, `local -a`,
`${var//pattern/repl}`, etc.) in the same pass rather than fixing arrays and
hitting the next bashism on a later run.

## Issue 8: `apk add`/`apk fix` reports "1 error" from busybox's own trigger even on a clean install

**Status: ROOT-CAUSED 2026-08-16 — it is a kernel `execve` bug, and the title
above misattributes it.** The busybox trigger now runs clean; the surviving "1
error" is a `#!/bin/sh` package script failing to start at all:

```
* bash-5.3.9-r1.post-upgrade: applet not found
ERROR: lib/apk/exec/bash-5.3.9-r1.post-upgrade: exited with error 127
(2/2) Reinstalling redis (8.8.0-r0)
Executing busybox-1.37.0-r31.trigger        <- this part is fine now
1 error; 697.4 MiB in 58 packages
```

**Cause: `exec_shebang` puts the *symlink-resolved* interpreter in `argv[0]`.**
`src/syscall/proc.rs` (`let interpreter = crate::vfs::resolve_symlinks(interpreter);`
shadows the as-written string, which is then pushed as `argv[0]`). `/bin/sh` is a
symlink to `/bin/busybox`, so a `#!/bin/sh` script execs busybox with
`argv[0] = /bin/busybox` and `argv[1] = <script>`. Busybox invoked *as* `busybox`
treats its first argument as an **applet name**, finds no applet by that name, and
exits 127. Linux passes the interpreter **exactly as written in the shebang** as
`argv[0]`, resolving the path only to load the image.

This breaks **every `#!/bin/sh` script**, not just apk's. Measured in-guest
2026-08-16 — the last two rows are the decisive control, identical but for `argv[0]`:

| invocation | result |
|---|---|
| `#!/bin/sh` script | `rc=127  a.sh: applet not found` |
| `#!/bin/busybox sh` script | `rc=0  OK` |
| `#!/bin/bash` script | `rc=0  OK` (real ELF; ignores `argv[0]`) |
| `busybox /tmp/a.sh` — what the kernel builds | `rc=127  a.sh: applet not found` |
| `busybox sh /tmp/a.sh` | `rc=0  OK` |

**`spawn.rs::resolve_shebang_chain` already gets this right** — it pushes the
unresolved interpreter into the argv prefix and resolves separately into
`elf_path`, and says so in its doc comment ("a shell must see the name it was
asked to run, not the symlink target"). So this is two implementations of one
rule that disagree; fix `exec_shebang` to keep the as-written string for
`argv[0]` and the resolved path for loading, and prefer sharing one
implementation. Issue 7 (`build.sh` "needs bash") is worth re-testing afterwards
— it may be the same bug rather than bash-specific syntax.

Original 2026-08-12 finding follows, kept because the reproduction is still the
fastest way to see it.

**Status (original): OPEN.** Found 2026-08-12, same session as Issue 7, while installing
packages (`xz`, then reproduced against `busybox` directly) inside a running
devbox-smoltcp guest over SSH.

### Symptom

```
$ apk add --no-cache xz
(1/1) Installing xz (5.8.3-r0)
Executing busybox-1.37.0-r31.trigger
1 error; 693.2 MiB in 57 packages

$ apk fix busybox
(1/1) Reinstalling busybox (1.37.0-r31)
  Executing busybox-1.37.0-r31.post-upgrade
Executing busybox-1.37.0-r31.trigger
1 error; 57 packages, 156 dirs, 2923 files, 693.2 MiB
$ echo $?
1
```

`apk` exits non-zero and prints "1 error" every time *any* package install
triggers busybox's own post-install trigger script — but nothing else looks
broken: `xz` itself works (`xz --version` succeeds right after), and
`/bin/sh -> /bin/busybox` resolves correctly (`readlink -f /bin/sh` →
`/bin/busybox`), so this is not a recurrence of Issue 5's dangling-symlink
bug (that one leaves `/bin/<applet>` pointing at a path that doesn't exist in
the guest; here the symlinks are fine).

### Not yet root-caused

`apk`/`apk fix` at default and `-v` verbosity both swallow whatever the
trigger script itself printed to produce "1 error" — neither run surfaced
the underlying failure over SSH, only the terse summary line. The user
flagged that this is inconsistent with prior sessions where `apk add`
returned a clean `OK: ...` with no error (e.g. `apk add git` earlier the same
session installed 687.7 MiB / 49 packages with `OK`, no trigger error), so
this looks like a real, if currently cosmetic, regression or environment
difference rather than an always-present property of this image.

### Reproducing

```bash
overlays/devbox/run-smoltcp.sh
ssh -o StrictHostKeyChecking=no -p 2222 root@localhost 'apk fix busybox'
```

### Next step, if picked up

SSH-level `apk -v` doesn't show the trigger's own stderr — needs either the
serial console (the trigger may be writing to a fd that doesn't survive the
ssh session) or extracting `busybox`'s trigger script from
`/lib/apk/db/scripts.tar.gz` and running it by hand to see what syscall or
condition it's hitting. Given the pattern of other Issue entries here
(missing `/proc` forwarding, unimplemented syscalls), a reasonable first
guess is the trigger calling something Akuma's syscall surface doesn't
support yet — but that's a guess, not a finding; confirm before acting on it.

## Issue 9: no IPv6 in the stack — `cargo` cannot reach crates.io, though `curl` can

**Status: OPEN, and it is a missing feature rather than a defect.** Found
2026-08-13 while trying to run a self-host kernel build on devbox-smoltcp.

### Symptom

`cargo build` / `cargo fetch` inside the VM never gets past the registry
fetch, failing every attempt in ~300 ms:

```
warning: spurious network error (16 tries remaining): [7] Could not connect to
server (Failed to connect to index.crates.io:443 after 289 ms: Could not
connect to server)
```

It exhausts all retries (`CARGO_NET_RETRY=20` was tried) and exits `101`
before compiling a single crate. `CARGO_HTTP_MULTIPLEXING=false` and
`CARGO_HTTP_TIMEOUT=120` do not help.

### Why it is confusing

The network is demonstrably fine, and `curl` reaches **the same host**:

```
$ curl -sS https://index.crates.io/config.json -o /dev/null -w '%{http_code} %{time_total}'
200 0.253642
$ curl -4 ... https://index.crates.io/config.json     ipv4=200
$ curl -6 ... https://index.crates.io/config.json     could not resolve host
```

So HTTPS, DNS and TLS all work — an in-VM `git clone --depth 1` of this repo
over HTTPS succeeds too (19 MB, rc=0), as does `apk add redis`.

### Cause

**The stack has no IPv6 at all, and never has.** `crates/akuma-net/Cargo.toml`
builds smoltcp with `proto-ipv4` only — no `proto-ipv6` — and there is not a
single `Ipv6`/`ipv6` identifier anywhere in `crates/akuma-net/src/`. It is an
unimplemented feature, not a regression.

`index.crates.io` is Fastly-hosted and its DNS answer is IPv6-heavy
(`nslookup` returns `2a04:4e42:600::649`, `2a04:4e42:200::649`, …). Standalone
`curl` recovers because its happy-eyeballs falls back to the A record; cargo's
bundled libcurl evidently attempts the AAAA addresses and gives up rather than
falling back, which is exactly the ~300 ms connect failure above.

### Consequence

**In-VM `cargo` builds against a live crates.io are blocked**, which blocks the
network path of the self-host flow (`../runbooks/selfhost-kernel-build.md`).
The runbook's `--offline` route with vendored deps is unaffected and remains
the way to self-host — that is what §2's `cargo vendor` step is for.

### Fix options, cheapest first

1. **Vendor the deps** and build `--offline`. No kernel work; already the
   documented self-host recipe.
2. **Suppress AAAA in the guest resolver** so cargo only ever sees A records.
3. **Implement `proto-ipv6`** in `akuma-net`. The real fix, and much the
   largest — it is a new address family through smoltcp, the socket layer and
   the syscall surface, not a feature-flag flip.

### Note for whoever picks this up

Do not diagnose this from `cargo`'s error text — "Could not connect to server"
reads like a routing or firewall problem and sends you looking at QEMU's user
networking. The decisive probe is `curl -4` vs `curl -6` against the failing
host, which separates "no route" from "no address family" in one command.

## Issue 10: rump devbox — ssh sessions reset at kex, no rump DHCP lease

**Status: FIXED 2026-08-13 —
[`RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md`](RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md).**
The "Where to look" conclusion below is **wrong** and is left as written for the
record. DHCP was never broken: `rump_server` logs to
`/var/log/box/0/rump_server.log` inside the image, not the console, and that file
shows the lease being taken (`dhcp: virt0: adding IP address 10.0.2.15/24`) on
every one of these boots. The missing console line is a logging destination.

The real cause was in sshd's direction after all: `RumpSocket` was the only fd
family `SharedFdTable::clone_deep_for_fork` did not take a reference on, so the
parent's post-`fork` `drop(stream)` sent a real NetBSD `close` and destroyed the
socket its own session child was about to speak SSH over. Fixed with a
`(box_id, rump_fd)` reference count in `rump_proxy`.

**Original report follows. Pre-existing — A/B-confirmed 2026-08-13, NOT a
regression** (that part was correct).
Found while verifying the `akuma-virtio` crate extraction (Phase 3 of
[`TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](TRIM_FAT_EMBARASSING_DUPLICATIONS.md)),
which is why the A/B below exists — the extraction was the initial suspect and
was cleared.

### Symptom

`overlays/devbox/run.sh` (rump devbox, `RUMP_NIC=1`, ssh on host `:2223`) boots
fine, but every ssh attempt dies before authentication:

```
$ ssh -p 2223 root@localhost uname -a
kex_exchange_identification: read: Connection reset by peer
```

Reproduced on three attempts spanning ~90 s after boot.

### What is working

Everything up to the network:

```
[rump] /dev/net/tap0 bound to NIC1 (bus.4), MAC 52:54:00:12:34:57
[RUMP-SP] rump-default: rump_server tid=8 registered as network
[herd] Started sshd (pid= 3)
```

`rump_server` is alive and busy (36,183 syscalls in 30 s), and sshd is in a
healthy idle accept loop — `accept=798` paired with `nanosleep=796` over 30 s is
the non-blocking poll cadence, not a spin. `[FUTEX-DUMP] table empty`, no
`PANIC`, no `[WILD-DA]`. The virtio drivers all come up
(`virtio-blk` slot 1 / 6144 MB, `virtio-rng` slot 2).

### The actual lead

**There is no rump DHCP lease in the log.** `run.sh` itself says to wait for one
("Once you see the rump DHCP lease + userspace sshd listening…"), and no lease
line ever appears — the only rump networking line is the tap0 bind above. Host
`:2223` forwards to *rump*:22, so with no address on the rump interface the
forwarded connection has nowhere coherent to land, which is consistent with a
reset at kex rather than a refusal.

So the suspicion is the **rump DHCP path**, not sshd. sshd resetting at kex is
the downstream symptom.

### A/B: it is pre-existing

Both arms were rebuilt from source and booted against the same `devbox.img`:

| arm | tree | ssh `-p 2223` |
|---|---|---|
| **new** | `9e22ea2` (akuma-virtio extracted) | `kex_exchange_identification: read: Connection reset by peer` |
| **clean** | `f09de7d` (parent; no `crates/akuma-virtio`, `src/virtio_hal.rs` still present) | **identical reset**, 3/3 attempts |

Both arms also produce the identical rump line set — `tap0 bound to NIC1
(bus.4)`, same MAC — and neither logs a DHCP lease. So the extraction is
cleared, and the fault predates it.

Corroborating: the *smoltcp* sibling is fully healthy on the **new** tree — ssh,
`curl` HTTP **and** HTTPS, `apk add`, an in-VM `git clone --depth 1`, and a
512 MB redis memtest all pass — so the shared virtio layer both stacks now use is
demonstrably fine. The fault is specific to the rump path.

CLAUDE.md lists the rump devbox as **deferred**, so this is not a routinely
exercised path and plausibly broke some time ago unnoticed. Nobody has bisected
it, so the breaking commit and date are unknown.

### Where to look

The rump DHCP path, not sshd. `rump_server` runs and sshd's accept loop is
healthy, but the rump interface never gets an address, and host `:2223` forwards
to *rump*:22.

## Issue 11: three spurious `[STACK-OVERFLOW]` lines on every `SMP=N>1` boot

**Status: FIXED, 2026-08-16** — via the first fix option below (paint the canary
in `adopt_current_as_core_idle`). Verified on devbox-smoltcp `SMP=4`: secondaries
online as idle tid 1/2/3, zero `[STACK-OVERFLOW]` lines. Full record, including
why `fill_stack_sentinel` must *not* be painted alongside it:
[`SMP_SECONDARY_IDLE_STACK_CANARY.md`](SMP_SECONDARY_IDLE_STACK_CANARY.md).

The investigation also turned up a second, independent bug in the same area —
`threading::init` trampling the slots secondaries had already adopted, which is
why the `spurious=0` row in the table below was passing for the wrong reason:
[`SMP_ADOPTED_IDLE_SLOT_CLOBBER.md`](SMP_ADOPTED_IDLE_SLOT_CLOBBER.md).

Original analysis follows, unchanged.

Not a devbox bug specifically — it fires on any `smp-shared`
boot with secondaries — but the devbox is where you meet it, because it is the
default multi-core image and it builds `no-tests` (so the self-test that would
have shown `spurious=0` isn't there to reassure you).

Every `SMP=4` boot prints, right after `[herd] Started sshd`:

```
[STACK-OVERFLOW] tid=1 ran off its 64KB kernel stack (base=0x402b5160) — kernel memory below it was corrupted
[STACK-OVERFLOW] tid=2 ran off its 64KB kernel stack (base=0x402c5160) — kernel memory below it was corrupted
[STACK-OVERFLOW] tid=3 ran off its 64KB kernel stack (base=0x402d5160) — kernel memory below it was corrupted
```

**All three are false.** The count is exactly `SMP - 1`, and the tids are exactly
the per-core idle slots.

### Why

`threading::adopt_current_as_core_idle` takes over a secondary's **boot stack** as
that core's idle thread. It registers the stack —
`pool.stacks[slot] = StackInfo::new(stack_base, stack_size)`, so
`stack.is_allocated()` is true — but it never **paints a canary**, because the
stack already existed and was never handed out by the allocating paths that paint.

`report_overrun_stack_canaries` then walks every allocated stack and reports any
whose canary is not intact. An unpainted canary is indistinguishable from a
smashed one, so each adopted idle slot reports once. Secondaries claim slots via
`claim_free_slot(1, MAX_THREADS)`, i.e. from 1 upward — hence tid 1..SMP-1.

Confirmed both directions:

| build | secondaries | result |
|---|---|---|
| `--release`, `SMP=1`, tests on | 0 | `spurious=0` from `test_stack_canary_overrun_is_reported` |
| devbox-smoltcp, `SMP=4`, `no-tests` | 3 | exactly 3 lines, tid=1,2,3 |

A/B-confirmed **pre-existing** at `79a18cd` vs `069f1f0` (the `akuma-primitives`
extraction): 3 lines on both, same tids, only the stack addresses differing.
The check itself is new — it arrived with
[`EXTREME_SSHD_KERNEL_STACK_OVERFLOW.md`](EXTREME_SSHD_KERNEL_STACK_OVERFLOW.md),
which gave a long-painted-but-never-checked canary its first caller — so this has
simply never been looked at on a multi-core boot.

### Why it is worth fixing rather than living with

1. **It is the loudest message the kernel has** ("kernel memory below it was
   corrupted"), on a clean boot, three times. That trains people to ignore it —
   which is the opposite of what a corruption detector is for.
2. **It masks real overruns on exactly those slots.** The reporter latches:
   `if CANARY_REPORTED_BASE[i].swap(stack.base) == stack.base { continue }`. The
   spurious report stores the base, so a *genuine* later overrun on the same
   per-core idle stack is silently skipped. The three cores' idle stacks are
   currently un-monitorable.

### Fix options

- **Paint the canary in `adopt_current_as_core_idle`.** Correct and small; the
  boot stack's base is already known there (`stack_base`), and it then gets real
  overrun coverage. Needs care that the painted words sit below anything the
  boot/trampoline context has already pushed.
- **Or exempt adopted slots** from the sweep with an explicit flag, which is
  honest but leaves three stacks unchecked.

The first is preferable — the whole point of the extreme-size autopsy was that a
kernel stack overrun is a real, live class in this tree.

## Issue 12: rump devbox — SSH stdin hangs at exactly 1 MiB (stale `/bin/sshd` on `devbox.img`)

**Status: OPEN, and it is an image-staleness bug, not a code bug.** The kernel is
fine and the fix already exists in `userspace/sshd`; `devbox.img` is carrying a
`/bin/sshd` built before it. Repopulate the image — do not go looking in
`rump_proxy.rs`.

**Pre-existing — A/B-confirmed 2026-08-13, NOT a regression.** Found while
verifying the `NoMem`/`DiscardMem` merge (§8 item 7 of
[`TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](TRIM_FAT_EMBARASSING_DUPLICATIONS.md)),
which was the initial suspect because it touches `src/rump_proxy.rs`'s
`ClientMem` impls, and was cleared.

### Symptom

On `overlays/devbox/run.sh` (rump devbox, `RUMP_NIC=1`, ssh on host `:2223`),
piping stdin into a remote command works up to 256 KiB and **hangs at 1 MiB**.
The client never returns; the remote command receives nothing at all (`wc -c`
prints an empty line, not a short count):

```
stdin    1024 B: OK
stdin   65536 B: OK
stdin  262144 B: OK
stdin 1048576 B: TIMEOUT, wc -c -> b''
```

Everything else on the same connection is healthy — 4 MiB **stdout** is
byte-for-byte correct, five sequential sessions connect cleanly, and the
sysproxy path itself is fine (`[RUMP-SP] box=0 proxy ready`). It is inbound
stdin only.

### Cause

[`SSHD_CHANNEL_WINDOW_NEVER_ADJUSTED.md`](SSHD_CHANNEL_WINDOW_NEVER_ADJUSTED.md):
sshd advertises a 1 MiB initial inbound channel window in
`SSH_MSG_CHANNEL_OPEN_CONFIRMATION` and never sends
`SSH_MSG_CHANNEL_WINDOW_ADJUST`, so the client correctly transmits exactly the
advertised 1 MiB and then waits forever. That was fixed in `userspace/sshd` —
but the fix ships as a **binary on the image**, and `devbox.img` has an older
`/bin/sshd` than `disk.img` does.

The same test on `disk.img` (`cargo run --release`, smoltcp) passes at 4 MiB.
Same kernel, same day, different image: that difference *is* the diagnosis.

### A/B, because "pre-existing" needs proving

`ddbfc55` (the `NoMem` merge) against its parent, both rebuilt and rebooted
through `overlays/devbox/run.sh`:

| build | stdin 256 KiB | stdin 1 MiB | stdout 1 MiB |
|---|---|---|---|
| `ddbfc55` (merge) | ok | **TIMEOUT** | md5 ok |
| `ddbfc55^` (before) | ok | **TIMEOUT** | md5 ok |

Identical. The merge is exonerated.

### Fix

Repopulate `devbox.img` so it carries the current `userspace/sshd`
(`overlays/devbox/bootstrap.sh`, or stage the rebuilt binary into the image).
Nothing in the kernel needs changing.

### The general trap

**`devbox.img` and `disk.img` carry independent copies of every userspace
binary, and they drift.** A userspace fix verified on one image is not present
on the other until that image is rebuilt, so the same test can pass on
`disk.img` and fail on `devbox.img` with no code difference at all. When a
userspace-shaped symptom appears on only one image, check the binary's age
before reading any kernel source.

This is also why the wrong VM got used in the first place: `src/rump_proxy.rs`
is only *executed* under `RUMP_NIC=1` on `overlays/devbox/run.sh`. A default
`cargo run --release` boot compiles it (`rump` is in the default feature set)
and then prints `[rump] BSP tap not available: tap: transport init failed (run
QEMU with RUMP_NIC=1)` — the code never runs, and a boot that looks completely
clean has verified nothing about it.

## Issue 13: `pwritev2` (nr 287) unimplemented — high-frequency `[ENOSYS]` console spam under build load

**Status: OPEN.** Found 2026-08-15 during the mapped-page premature-free
verification runs (`MAPPED_PAGE_PREMATURE_FREE_FIX.md`). Not investigated
beyond decoding the syscall — adding here so it doesn't get lost.

### Symptom

The console fills with lines like:

```
[ENOSYS] nr=287 pid=54 args=[0x4, 0x203fff9490, 0x1]
```

repeating at high frequency while a build workload runs. nr 287 on the
aarch64 Linux ABI is `pwritev2(fd, iov, iovcnt, pos_lo, pos_hi, flags)` —
here `fd=4, iovcnt=1` (the offset/flags args fall outside the 3-arg print;
the common calling pattern is `pos=-1, flags=0`, i.e. "plain `writev`, just
probing for the newer syscall"). The dispatcher's catch-all
(`src/syscall/mod.rs`, the `_ =>` arm) returns `-ENOSYS` and prints one line
per attempt.

### Why it (mostly) doesn't matter — and why it still does

Nothing is broken: every libc/runtime that issues `pwritev2` falls back to
`writev` (nr 66, implemented) on `ENOSYS`, so the writes succeed. But this
caller is **not caching the ENOSYS** — it re-probes on every write, so each
write pays a wasted syscall round-trip plus a console print, and under load
the print is the expensive half (console output is serialized and not free
on this kernel). The spam also buries real diagnostics in the log.

### Next step, if picked up

1. Identify the caller: grep the console log for the pid's `execve` line
   (the syscall trace prints binary + argv per spawn).
2. If warranted, add a `PWRITEV2` dispatcher arm: `flags == 0 && pos == -1`
   → forward to `fs::sys_writev`; `flags == 0 && pos >= 0` → a positional
   variant; nonzero flags → `-EOPNOTSUPP` (Linux's own convention for
   unsupported `RWF_*` flags). Same treatment for `preadv2` (286) if it
   surfaces. Per repo convention, the syscall change needs a boot-suite
   self-test in `src/process_tests.rs`.

## Issue 14: `sys_spawn` could not run `#!` scripts — every OCI image's Entrypoint is one

**Status: FIXED 2026-08-16.** Found running the official `redis:alpine` image
in a box.

### Symptom

```
~ # box run --rm -d redis:alpine redis-server --port 4444
box: running '/usr/local/bin/docker-entrypoint.sh' in redis-alpine-53265326 (7 layers, ...)
box run: failed to spawn /usr/local/bin/docker-entrypoint.sh
```

The file exists in the overlay, is executable, and `--entrypoint
/usr/local/bin/redis-server` on the same image works — so it is not the image,
the pull, or the overlay.

### Cause

`do_execve` has always handled `#!` (`exec_shebang`, `src/syscall/proc.rs`).
**`spawn_process_with_channel_ext` never did.** Everything that goes through
Akuma's SPAWN abi rather than exec — herd's services and all of `box run` —
could therefore only start real ELF binaries.

That is not a corner case: `redis`, `postgres`, `mysql` and `nginx` all ship a
`docker-entrypoint.sh` as their Entrypoint, so *no* official image could run
under its own entrypoint.

### Fix

`resolve_shebang_chain` in `crates/akuma-exec/src/process/spawn.rs`, called
after path resolution and **inside** the namespace override — a container's
`/bin/sh` lives in the image's layers, so reading the shebang from box 0's view
would find the wrong interpreter or none. Follows up to 4 hops, like Linux.

Two things came out of writing it:

- **`exec_shebang` had an `argv[0]` bug**, found independently by another agent
  and fixed in the same pass: it shadowed the interpreter-as-written with its
  symlink-resolved target and used the *resolved* path as `argv[0]`. Linux uses
  the name from the `#!` line. On a busybox system that is fatal rather than
  cosmetic — busybox dispatches entirely on `argv[0]`, so `#!/bin/sh` ran
  `/bin/busybox` with `argv[0]="/bin/busybox"` and busybox never knew it was
  meant to be a shell. The decisive experiment was `busybox /tmp/a.sh` vs
  `busybox sh /tmp/a.sh`: same binary, same script, differing only in `argv[0]`.
- Both paths now share one parser (`parse_shebang`) and one argv rule
  (`shebang_hop`). Two implementations of one rule is how they diverged.

Tests: `shebang_tests` (host, in `spawn.rs`) for the parsing and argv
construction; `spawn_resolves_a_shebang_script` (boot suite,
`src/process_tests.rs`) against the real VFS.

## Issue 15: privilege-dropping entrypoints re-exec forever — no per-process credentials

**Status: OPEN.** The blocker between "`box run redis:alpine` with
`--entrypoint`" and "`box run redis:alpine`".

### Symptom

`box run --rm -d redis:alpine redis-server --port 4444` starts, prints
`Started PID 8`, and then nothing happens at all: no listener, no log output,
no exit. `box ps` shows the container alive.

### Cause

`docker-entrypoint.sh`:

```sh
if [ "$1" = 'redis-server' -a "$(id -u)" = '0' ]; then
	find . \! -user redis -exec chown redis '{}' +
	exec setpriv --reuid=redis --regid=redis --clear-groups -- "$0" "$@"
fi
exec "$@"
```

`setpriv` now succeeds (see the chain below), but Akuma has **no per-process
credentials**: `setresuid` is an accepting no-op and `getuid`/`geteuid` hardcode
0. So the re-exec'd script sees `id -u` = 0 *again*, takes the same branch, and
re-execs itself under `setpriv` forever. It never reaches `exec "$@"`.

This is the general trap of a silently-succeeding credential syscall, not a
Redis quirk — "drop privileges and re-exec" is the standard entrypoint shape.

### What it took to get this far

Four separate gaps, each of which had to be closed before the next was visible:

| Failure | Cause | Fix |
|---|---|---|
| `exec: line 184: redis-server: not found` | `DEFAULT_ENV`'s `PATH` was `/usr/bin:/bin`; images install under `/usr/local/bin` | Full Linux search order in `crates/akuma-exec/src/process/types.rs` |
| `setpriv: getresuid failed: Function not implemented` | `getresuid`/`getresgid`/`getgroups` undispatched | Implemented — everything is root, so all report 0 |
| `setpriv: activate capabilities: No error information` | `/proc/self/<anything>` did not resolve — the VFS never chased the `self` symlink, so procfs saw the literal string `self/status`. Same gap that blocked Redis from starting at all in `LONG_ROAD_TO_REDIS.md` | `resolve_self` in `src/vfs/proc.rs`; `Cap*` lines added to `/proc/<pid>/status` |
| *same message, still* | `capget` returned success for **any** header version. Linux answers an unknown version by writing back the supported one and returning `EINVAL` — that is a negotiation, and libcap-ng performs it by calling `capget` with version 0 | Real version negotiation + a full-root capability set in `sys_capget` |

`No error information` is musl's `strerror(0)`: the failing call returned -1
without setting errno, i.e. it was **not** a syscall — it was libcap-ng. Two
plausible fixes (stub `capset`, add `Cap*` to procfs) each "should have" fixed
it and neither did. Verify the fix, not the theory.

### Fix

Add `uid`/`gid` to `Process`; have the `get*` family read them and the `set*`
family write them; inherit across fork/exec. **No enforcement is needed** — the
kernel would simply report the identity a process asked for, which is enough to
break the loop. A couple of hours. The risk is second-order: tools that behave
differently as non-root, and file permission checks this kernel does not do.

Until then, `--entrypoint /usr/local/bin/redis-server` skips the script:
[`../runbooks/run-redis.md`](../runbooks/run-redis.md) §3.

## Issue 16: socket budget caps concurrent clients at ~20-50

**Status: OPEN, a sizing limit rather than a defect.** Found benchmarking Redis.

`redis-benchmark -p 4444 -c 50` fails outright:

```
Could not connect to Redis at 127.0.0.1:4444: Can't create socket: No file descriptors available
```

`-c 20` is clean (4000-4500 rps), and back-to-back runs at `-c 20` can hit it
too, so the effective ceiling depends on recent history rather than on the
number alone.

That message is `alloc_socket` returning `None` → `EMFILE`, from two caps:
`akuma_net::socket::MAX_SOCKETS` (128 `KernelSocket` entries) and
`smoltcp_net::MAX_SOCKETS` (256 with `many-sessions`). Every listener
pre-allocates `MAX_BACKLOG` (32 with `many-sessions`) smoltcp sockets at
`TCP_RX_BUFFER_SIZE + TCP_TX_BUFFER_SIZE` = 32 KB each — ~1 MB of heap held for
the listener's whole life — and closed sockets sit in `pending_removal` for a
cooldown before their slots come back, which is why consecutive runs are worse
than a cold one.

Fix would be a lazily-grown backlog (create listening sockets on demand up to
the cap instead of all 32 up front) plus a configurable total. Both caps are
compile-time constants today.

## Background

- [`SSHD_CHANNEL_WINDOW_NEVER_ADJUSTED.md`](SSHD_CHANNEL_WINDOW_NEVER_ADJUSTED.md)
  — Issue 12's root cause: the missing `SSH_MSG_CHANNEL_WINDOW_ADJUST`, and why
  its 1 MiB limit coincided with `ProcessChannel::MAX_BUFFER_SIZE` so the two
  defects hid each other.
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

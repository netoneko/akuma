# Bun hangs ~10 s in: a signal handler never runs because a futex wait ignores SA_RESTART signals (FIXED)

**Status: FIXED 2026-11-10 (amd64 `FUTEX_WAIT` now returns `EINTR` for any deliverable pending signal).** Started as "interactive `claude` stops reading keyboard
input on the ryzen"; the evidence below says it is **not** stdin, a tty, or epoll. Updated as
evidence arrives; every section is dated.

## Symptom

Claude Code 2.1.301 (a Bun 1.4.3 single-file executable) on Akuma amd64 stops responding to input
a few seconds in; typed text queues in the pty and echoes after the process is killed. The same
thing happens to a **plain Bun script that only reads stdin**, which is how it was isolated.

## What it is not (2026-11-10)

| Suspect | Result |
|---|---|
| ryzen-only / core-1 timer | Wrong: reproduces on the trashcan (4 cores) and in Firecracker/KVM guests with >= 4 vCPUs; never with 1-2 vCPUs |
| `Bun.stdin.stream()` vs `process.stdin` | No effect: both stall |
| stdout `/dev/null` vs tty | No effect on whether it stalls |
| Python `epoll` ET/LT probe, C probe copying Bun's tty read sequence (`preadv2(RWF_NOWAIT)` -> `poll(0)` -> `read`, non-NULL sigmask, `EPOLLIN|EPOLLHUP`, 8 idle threads, MOD / DEL+ADD re-arm) | All pass, in the same 4-vCPU guest where Bun stalls ~half the time |
| Kernel version string (`0.0.9` < 5.11 makes Bun skip `epoll_pwait2`) | Bun falls back to `epoll_pwait` either way; not a trigger |
| "Stalls ~20 keys / ~10 s after start" | Not a fixed trigger: with the first key at 2/4/6/8/10/14 s every run passed on the trashcan, and the same config had stalled every time earlier. Flaky, state-dependent |

Stall matrix on the trashcan (`c523598a`, keys every 0.2 s; scratchpad `stdin_test2.js`):
data/no-timer/tty STALL, data/no-timer/null STALL, data/timer/tty OK x3, data/timer/null STALL,
stream/no-timer/tty STALL, stream/no-timer/null STALL, stream/timer/tty STALL, stream/timer/null OK x1.
No single factor separates passes from stalls.

## Evidence that it is a futex hang (2026-11-10, tracing kernel in a 4-vCPU KVM guest)

Kernel = `c523598a` + a lock-free trace ring in `akuma-threading` (park / waker / wake-pass /
resume events, per-thread enable, printed by the idle loop's 30 s block) + `DUMP_FUTEX_WAITERS` and
`DUMP_SLOT_TABLE` flipped on in `crates/akuma-kernel-amd64/src/sched.rs`. In a stalled Bun (tgid 37):

- **Every thread of the process is parked in a futex wait** (`park=futex.rs:297`, untimed, woken only
  by the 1 s untimed-park backstop and going straight back to sleep; one worker is in a timed
  wait at `futex.rs:299`). Nothing in the process is runnable.
- **The main thread** is queued on `key tgid=37 uaddr=0x7fffffffd024` — an address **on its own
  stack** — for ~22 s at the time of the dump, i.e. since ~9.8 s after process start, which is
  exactly when the stdin test's `GOT` lines stop.
- Its last `epoll_pwait` returned normally (a timeout return) before it entered the futex. It
  never left epoll "stuck"; the earlier theory of a lost epoll/pty wake was wrong.
- The other threads (`HeapHelper` x3, `collector`, `mi-scavenger`) wait on heap futexes
  (`0x3c13..8b4`, `0x513b960`, `0x52312b0`). The shape is JavaScriptCore's GC thread handshake
  (main waits on a stack-allocated latch for the collector/helpers) with a wake that never lands.

## No-tty reproduction

`scratchpad/gc_test.js`: a 50 ms `setInterval` that allocates objects and calls `Bun.gc(true)` every
5th tick, writing a progress file. 4-vCPU guest: **hangs at ~11 s in 2 of 3 runs** (`timeout 40`
-> rc 143, progress file frozen at n=215/216). Trashcan: **passes 9 of 9**. So the
keystrokes were only a trigger (allocation -> GC at a particular moment); the hang itself needs no
terminal.

## Update 2026-11-10 (futex trace of the GC-script hang, main thread only)

Tracing every futex call of the process's *main* thread (helper threads rename themselves with
`prctl(PR_SET_NAME)`, so a comm filter missed them — next build keys on tgid) in a hung `gc_test.js`
run, 4-vCPU guest:

- The main thread broadcasts to the collector/helpers (`FUTEX_WAKE_PRIVATE` on heap futexes
  `0x52312b0`, `0x3c0..984`, `0x513b960` with count 65535 -> woke 1, then 0), then enqueues
  `FUTEX_WAIT_PRIVATE` on a **stack** latch (`0x7fffffffd054`; an earlier identical round used
  `0x7fffffffce24` and returned 0 immediately after one `FUTEX_WAKE` from a helper).
- Within the same millisecond something readied the thread (`k2` waker with state WAITING, `k4`
  sticky resume) **but the wait did not return** — the waiter stayed queued (the idle-loop futex
  dump still lists it), so that wake was not a `FUTEX_WAKE` on the latch's key: a spurious ready,
  after which the wait loop re-tested, found itself still queued, and re-parked.
- Then nothing for 14 s except the 1 s untimed-park backstop (filtered out of the trace) until the
  test's `timeout` killed it (`ret -4`, EINTR).

So the helper's `FUTEX_WAKE` on the latch either never happened or never matched. Next: capture
the helpers' futex calls (same tgid) to see whether the latch wake was issued and on what address.

## Root cause and fix (2026-11-10)

`BUN_JSC_useConcurrentGC=0` makes the GC-script hang disappear (0/6 hangs; baseline 5/6, one marker
thread 6/6, JIT off 5/6), which points at the concurrent collector — and JavaScriptCore's collector
**suspends the mutator with a signal**: `pthread_kill(main, SIGUSR1)`, then waits for main's handler to
post a semaphore. The main thread is parked in `FUTEX_WAIT` (on a stack latch) when the signal
arrives. The signal wakes it (the spurious `k2`/`k4` ready in the trace) but `crates/akuma-kernel-amd64/src/futex.rs`'s
wait loop only asked `should_interrupt_blocking_syscall()`, which **deliberately skips `SA_RESTART`
handlers** ("not reporting it makes the loop take another pass — a restart"). That is fine for
`read`/`write` but wrong for a loop that never returns to user mode: delivery happens at syscall
return, so the handler never runs, the collector waits forever, and the whole process is parked.
Linux runs the handler and restarts the wait. WTF installs its suspend handler with `SA_RESTART`.

Minimal reproducer (no Bun, `scratchpad/srfutex.c`): thread A blocks in `FUTEX_WAIT`; thread B
`pthread_kill`s it and waits 3 s for the handler. Before: `SA_RESTART=1 handler_ran=0`,
`SA_RESTART=0 handler_ran=1`. After: both run the handler and A's wait returns `-4`.

Fix: in the amd64 futex wait loop, a pending signal not blocked by the thread's mask
(`pending_signals_raw(me) & !thread_signal_mask() != 0`) ends the wait with `EINTR`, whatever
`SA_RESTART` says. Every futex caller (musl, Rust `std`, WTF) already loops on `EINTR`. The aarch64
path already did this (`akuma-syscalls-glue/src/sync.rs`: `signal_pending`), so it never had the bug.

Verified in the 4-vCPU KVM guest: GC script **12/12 pass** (was ~50-80% hang), original stdin repro
**8/8 pass** (stalled at ~10 s almost every time), `srfutex` passes in both modes.

Other blocking waits that use `should_interrupt_blocking_syscall` alone (pipe/socket/poll families)
keep the `SA_RESTART` skip; a program that signals a thread parked there with a `SA_RESTART` handler
and waits for the handler hits the same shape. Not observed; audit if one shows up.

Open oddity: on the trashcan the same `srfutex 1` run reports `handler_ran=1` (A returns `-4`) with the
same `c523598a` version string, i.e. its kernel already behaved as fixed for that test while the Bun
stall still reproduced there with stdin keys. Not explained.

## Candidate causes (superseded)

- A wake lost on the `FUTEX_CMP_REQUEUE` / condvar path musl uses for WTF's condvars
  (`crates/akuma-kernel-amd64/src/futex.rs`, `akuma-syscalls-sync`), e.g. the waiter moved to a key the later
  `FUTEX_WAKE` does not reach.
- The enqueue/park/resume protocol under SMP (`akuma_threading::schedule_blocking`,
  `publish_waiting_and_take_pending_wake`, `x86_wake_pass`) — a wake that dequeues the waiter but
  does not ready the thread (the dump shows the waiter *still queued*, so the wake never reached
  its key; this makes a missing `FUTEX_WAKE` call or a key mismatch likelier than a park race).
- Genuine application deadlock exposed by timing (less likely: every thread is parked and Linux
  runs the same binary fine).

## Also found, unrelated to this hang

The idle loop runs the 30 s `[PSTATS]` block **while holding the BKL**, so on a box with many
processes that dump spins other cores for seconds (`bkl_max_wait_spins` reached 2.7e8 on the ryzen).
Separate defect; not the cause here (the stall also happens between dumps).

## Rig notes

- KVM guest: `/root/fc-p3.sh <kernel.bin> 4 3072` on Pop (ryzen) — a throwaway COW snapshot of p3.
  **Every boot wipes the guest's `/tmp` and the `/src/.../akuma-c523` worktree**, and the guest
  clock reads 1970, so a patched file needs `touch -d 2026-11-10 ...` or cargo will not rebuild it.
  Build inside the guest with `kbuild -j 4 --online --features no-tests`, copy the kernel out
  *before* rebooting the guest.
- Do not run `taskset` (affinity experiments) on the ryzen's metal Akuma: one preceded a hard lockup
  and a watchdog reset to Pop.


## Background

Memory notes `project_ryzen_bun_stall_core1_2026_11_10`; related archive docs `BUN_MISSING_SYSCALLS.md`,
`AMD64_CLAUDE_LOGIN_INPUT_STUCK.md`, `AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §13 (untimed
`FUTEX_WAIT` waiters waking on the backstop with no new syscall — the same shape).

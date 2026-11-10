# Bun hangs ~10 s in: main thread parked on a futex nobody wakes (OPEN)

**Status: OPEN — root cause not found.** Started as "interactive `claude` stops reading keyboard
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
`DUMP_SLOT_TABLE` flipped on in `amd64/src/sched.rs`. In a stalled Bun (tgid 37):

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

## Candidate causes (not yet tested)

- A wake lost on the `FUTEX_CMP_REQUEUE` / condvar path musl uses for WTF's condvars
  (`amd64/src/futex.rs`, `akuma-syscalls-sync`), e.g. the waiter moved to a key the later
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

## Next

Trace every futex operation of the Bun process (wait / wake / requeue with address, thread, result,
woken count) into the ring and find the wake that should have reached `uaddr=0x7fffffffd024`'s key.

## Background

Memory notes `project_ryzen_bun_stall_core1_2026_11_10`; related archive docs `BUN_MISSING_SYSCALLS.md`,
`AMD64_CLAUDE_LOGIN_INPUT_STUCK.md`, `AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §13 (untimed
`FUTEX_WAIT` waiters waking on the backstop with no new syscall — the same shape).

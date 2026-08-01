# Recover a wedged VM

What to do when the devbox (or any Akuma VM) is hung, pegged at 100% CPU, or
SSH-unresponsive. Goal: preserve logs and diagnose before killing.

## Step 1: Is it wedged or just busy?

| Signal | Verdict |
|---|---|
| QEMU process at ~100% CPU, new SSH connects but commands hang | **Wedged** (likely a spinning thread — see [`debug-devbox.md`](debug-devbox.md) "VM pegged at 100% CPU") |
| QEMU at ~100% CPU, SSH banner-exchange timeout during a known CPU job (rustc, tcc) | **Starved, not wedged** — the rump thread/sshd can't get a timeslice. Wait, or run lighter. |
| QEMU at ~0% CPU, no log output | True hang (deadlock) — see "Extract logs" below. |
| New SSH sessions refuse entirely | Port conflict / sshd dead — `pkill -9 qemu-system-aarch64`, check `RUMP_SSH_PORT`. |

A VM where **new SSH sessions still connect** is not fully dead — the kernel
is scheduling, but stuck processes aren't cleaning up. Don't kill it yet; grab
diagnostics.

## Step 2: Grab diagnostics before killing

If SSH still connects at all, from **another** SSH session:

```bash
ps                                     # each proc's x30/elr + current_syscall
cat /var/log/box/0/rump_server.log     # rump server's own log
top                                    # CPU attribution
```

The `ps` builtin output is the highest-signal diagnostic: the saved kernel
resume point (`x30`/`elr`) and `current_syscall` show exactly where each
thread is stuck.

If SSH won't connect, the **QEMU serial console** (stdout) is your only log.
Re-run boot with output redirected:

```bash
overlays/devbox/run.sh > devbox.log 2>&1
grep -a "THR-DUMP\|panic\|FAULT\|deadlock" devbox.log
```

(`grep -a` — QEMU/HVF emits a control byte that makes plain `grep` treat the
log as binary.)

## Trap: ad-hoc debug prints can manufacture a fake wedge

Before trusting a wedge you're seeing *while adding your own
`safe_print!`/`log::` tracing*, rule this out first — it cost real time on
`EXEC_CHANNEL_LARGE_OUTPUT_TRUNCATION.md` (2026-08-01), where the first two
hypotheses chasing a genuine deadlock were both artifacts of the tracing
itself:

- `console::print`/`safe_print!` → `console::emit` wraps every byte write in
  its **own** `with_irqs_disabled`. A print call placed inside code that is
  *already* holding a spinlock (especially one also under IRQs-disabled) adds
  real work — and its own IRQ-disable/UART-write loop — to a region that was
  meant to be short. Under heavy call volume (a print per loop iteration in a
  hot retry/wake path) this can visibly stall progress or overflow the host
  console pipe, producing the exact symptom of §"When to suspect a deadlock"
  below (100% CPU, log goes silent) even when the underlying logic is correct.
- The fix: never print from inside a locked/IRQs-disabled critical section.
  Print only at call sites where no lock is held (function entry, after a
  lock guard has dropped) and throttle high-frequency loops (print every
  Nth iteration, not every one). If a "hang" only reproduces with your own
  tracing added and disappears when you strip it back out, that's your
  answer — rebuild clean and re-test before spending more time on the
  original hypothesis.
- This does **not** mean tracing is safe to skip entirely — it's how the
  *real* bug in that investigation was eventually found (once the tracing was
  placed correctly). It means: add prints minimally, outside locks, and
  always confirm a suspected hang still reproduces with tracing fully removed
  before concluding it's real.

## Step 3: Enable diagnostics for the next boot

Flip these in `src/config.rs` and rebuild (see
[`../reference/subsystems/config-flags.md`](../reference/subsystems/config-flags.md)):

- `DEADLOCK_THREAD_DUMP_ENABLED = true` — dumps all threads on suspected deadlock.
- `FUTEX_DBG_ENABLED = true` — futex diagnostics.
- `RUMP_SP_TRACE = true` — one line per proxied socket syscall.
- `SYSCALL_DEBUG_INFO_ENABLED = true` — full syscall trace.

Boot under TCG for faithful PC reporting (HVF gdbstub misreports PC as the
exception-vector entry):

```bash
HVF=0 overlays/devbox/run.sh
```

Or attach a debugger:

```bash
GDB=1 overlays/devbox/run.sh     # QEMU gdbstub on :1234
lldb -p :1234
```

## Step 4: Kill and restart

When you've captured what you can:

```bash
pkill -9 qemu-system-aarch64
```

If booting from a snapshot (`SNAPSHOT=1`), disk state is preserved across
kills; otherwise the disk is as last written.

## Known wedge modes (current)

| Mode | Signature | Status | Workaround |
|---|---|---|---|
| Shell pipeline `cmd \| head -N` | `[signal] tkill(tid=X, sig=13)` then ~99% CPU; new SSH connects but stuck procs never clean up | **Open** (SIGPIPE delivery to writer spins) | Redirect to a file, not a pipe. |
| CPU-bound load starves SSH | Banner-exchange timeout during rustc/tcc codegen; QEMU pegged | **Open** (scheduling weight) | Run lighter; wait out the codegen. |
| tap-poll busy-spin at idle | ~100% CPU at idle; `meow` first-token ~78s | **FIXED 2026-07-06** (`rumpcomp_tap.c`) | — |
| BSP idle busy-yield | ~100% CPU at idle on a clean boot | **FIXED 2026-07-07** (`idle_halt()`) | — |
| Scheduler/timer freeze under `execve` load | VM unresponsive under heavy execve | **FIXED** (`sgi_scheduler_handler_with_sp` now `POOL.try_lock()`) | — |
| Single rump client slot wedge | One rump box client blocked holds the slot | **Open** | Avoid concurrent heavy rump box clients. |
| `ProcessChannel` mixed-IRQ-discipline lock | New lock user (`check_set_writer`) took `pollers` under `with_irqs_disabled`; every pre-existing caller (`add_poller`, wake loops) took it with IRQs enabled — a preempted unprotected holder can never resume once another thread spins on the same lock with IRQs off | **FIXED 2026-08-01** (all `ProcessChannel.pollers`/`.buffer` access now consistently `with_irqs_disabled`) | See `userspace/sshd/docs/EXEC_CHANNEL_LARGE_OUTPUT_TRUNCATION.md` §7.2 |

See `archive/KNOWN_ISSUES.md` for the full history.

## When to suspect a deadlock (vs a spin)

- QEMU at **~0% CPU** with no log progress → deadlock (threads blocked, no
  spin). Enable `DEADLOCK_THREAD_DUMP_ENABLED` and look for `[THR-DUMP]`.
- QEMU at **~100% CPU** with repeating log lines → spin. `ps` shows a thread
  whose `elr` isn't advancing.

## Background

- `archive/KNOWN_ISSUES.md` — issue register (§10-11 are the devbox CPU bugs).
- `archive/FREEZE_INSTRUMENTATION_PLAN.md` — instrumentation plan for freezes.
- `archive/OPTIONAL_SMOLTCP.md` — rump-path failure modes.

# herd misclassifies signal deaths as clean exits

**Status: FIXED (2026-08-08), as proposed in §4.** Found while fixing
`userspace/sshd`'s exit reporting
([`../../sshd/docs/EXIT_STATUS_FIX.md`](../../sshd/docs/EXIT_STATUS_FIX.md)),
which hit the same root cause one layer down. The analysis below is from reading
the code paths; the restart-loop behaviour was **never** observed in a running VM
(see §6 — the verification plan is still unrun).

What landed:

| Piece | Where |
| --- | --- |
| `kill_signal(pid, sig)` + `SIGTERM`/`SIGKILL` constants | `userspace/libakuma/src/lib.rs` |
| Restart policy as a pure, host-tested decision (`Policy`/`Exit`/`Outcome`/`classify`) | `userspace/herd/src/exit.rs` (new) |
| Host-test target for it (`no_std` dropped under `cfg(test)`, `libakuma` optional) | `userspace/herd/src/lib.rs` + `Cargo.toml` (new lib target) |
| `check_process_exits` reaps with `waitpid_status`, classifies via `classify`, records `shell_code()` | `userspace/herd/src/main.rs` |
| `stop_service` sends a real SIGTERM | `userspace/herd/src/main.rs` |

`libakuma::kill` was left alone (§4) and now carries a doc comment saying plainly
that it only probes. Run the unit tests with:

```bash
cd userspace && cargo test -p herd --lib --no-default-features \
  --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```

## 1. Root cause

`libakuma::waitpid` returns `WEXITSTATUS` only. A process killed by a signal has
nothing in the high byte of its wait status, so it decodes as **exit code 0** —
identical to a clean success.

The kernel is not at fault and never was. It records `exit_code = -(signal)` and
`encode_wait_status` puts a signal in the low 7 bits, precisely so the two cases
stay distinguishable. `waitpid` throws that away; `waitpid_status` (added for the
sshd fix) preserves it.

So to herd, a service that segfaults, aborts, or is `kill -9`ed is a service that
exited 0.

## 2. What that does to the restart policy

`check_process_exits` (`src/main.rs`) branches on the exit code:

```rust
} else if exit_code != 0 && svc.config.restart {
    // schedule PendingRestart: honours restart_delay_ms,
    // increments restart_count, trips max_retries -> Failed
} else {
    // "Clean exit"
    svc.state = ServiceState::Stopped;
    svc.restart_count = 0;
}
```

A crashed service takes the second branch. **It is not stranded** — the tempting
conclusion, and the wrong one. `start_stopped_services` revives anything in
`Stopped`, so it does come back. It comes back through the wrong door:

| | crash today (via `Stopped`) | intended (via `PendingRestart`) |
| --- | --- | --- |
| `restart_delay_ms` | skipped — respawn on the next poll | honoured |
| `restart_count` | reset to 0, never incremented | incremented |
| `max_retries` | never consulted | trips → `Failed` |
| `last_exit_code` | recorded as `0` | the real code |

The consequences, in order of how much they'd hurt:

1. **A service that crashes on startup becomes a hot restart loop.** No delay, no
   retry ceiling, so it respawns every poll interval forever. The
   `restart_delay`/`max_retries` backoff exists for exactly this and is bypassed.
2. **`ServiceState::Failed` is unreachable for crashes.** A permanently broken
   service never gets marked failed, so nothing reports it as such.
3. **`last_exit_code` records a lie** — herd's own status says a crash succeeded.

Note what is *not* signal-specific: `Stopped` services are revived regardless of
`config.restart`, so a `restart = false` service is respawned after any exit,
clean or not. That looks like a separate pre-existing design question about what
`restart = false` is meant to mean, and this doc does not propose changing it.

## 3. Second bug: `stop_service`'s kill is a no-op

Found while checking whether the fix in §4 would make an operator-initiated stop
look like a crash and trigger a restart.

`libakuma::kill` takes no signal and hardcodes 0:

```rust
pub fn kill(pid: u32) -> i32 {
    syscall(syscall::KILL, pid as u64, 0, 0, 0, 0, 0) as i32
}
```

Syscall 302 dispatches to `proc::sys_kill(pid, sig)` (`src/syscall/mod.rs:772`),
whose first act is:

```rust
// sig=0 is a "does the process exist?" probe — don't actually send anything.
if sig == 0 { return if lookup_process(pid).is_some() { 0 } else { ESRCH }; }
```

So `stop_service` **probes** the process and then, believing it stopped it, clears
`svc.pid` and marks the service `Stopped`. The process keeps running, now
unsupervised — and `start_stopped_services` will happily start a *second* copy.

This is independent of the exit-code bug and arguably the more serious of the two.

## 4. Proposed fix

**`libakuma`** — add a signal-taking kill; leave `kill` alone (call sites exist
outside herd) or make it delegate with `SIGTERM`, which is a behaviour change and
should be its own decision:

```rust
/// Send `sig` to `pid`. `sig = 0` only probes for existence.
pub fn kill_signal(pid: u32, sig: u32) -> i32 {
    syscall(syscall::KILL, pid as u64, sig as u64, 0, 0, 0, 0) as i32
}
```

**herd** — reap with `waitpid_status` and classify:

```rust
if let Some(status) = waitpid_status(pid) {
    exited.push((name.clone(), status));
}
...
// A signal death is a failure even though WEXITSTATUS is 0.
let failed = status.signaled() || status.exit_code() != 0;
svc.last_exit_code = Some(status.shell_code()); // 128+sig for a signal death

if svc.config.oneshot {
    /* unchanged */
} else if failed && svc.config.restart {
    /* PendingRestart, as today */
} else {
    /* Stopped */
}
```

`shell_code()` (128 + signal, the `$?` convention) is what should be recorded and
printed, so a SIGSEGV shows as 139 rather than 0.

**`stop_service`** — send a real signal, and keep it distinguishable from a crash:

```rust
let _ = kill_signal(pid, 15); // SIGTERM
```

It already clears `svc.pid` and sets `Stopped` before any reap, and
`check_process_exits` only considers `Running` services holding a pid — so the
stopped service is never reaped and the §4 change cannot turn a stop into a
restart. That ordering is load-bearing; preserve it. A follow-up may want
SIGTERM-then-SIGKILL with a grace period, which *would* need a "stopping"
state to stay unambiguous.

## 5. Edge cases considered

- **`oneshot`** — checked before the failure branch, so unaffected either way.
- **Catchable signals** — busybox `sh` handles SIGTERM/INT/QUIT/SEGV and exits
  130, a genuine clean exit with no signal to report. Only uncatchable SIGKILL
  (and signals a service does not handle) reach herd as `signaled()`. A test that
  expects `kill -15` to look like a crash will mislead you.
- **Cross-core (`core = N`) services** — have no local pid and are never reaped
  here; out of scope.

## 6. Verification plan

**Still not run** — the fix is covered by the unit tests in
`userspace/herd/src/exit.rs` (signal-death-is-a-failure, the `max_retries`
ceiling being reachable at all, oneshot, `restart = false`), which pin the
decision but not the live restart timing or the `stop_service` kill. The
in-VM checks below remain outstanding.

Suggested, in an isolated VM
(`../../sshd/docs/EXIT_STATUS_FIX.md` §5 has the isolation recipe):

1. A service that segfaults immediately. **Before**: respawns every poll with
   `exited with code 0`, `restart_count` pinned at 0, never `Failed`. **After**:
   respawns on `restart_delay_ms`, count climbing, `Failed` at `max_retries`.
2. Same with `max_retries = 3` — assert exactly 3 restarts, then `Failed`.
3. `herd stop <svc>` — assert the process is actually gone (`ps`), which fails
   today, and that no second copy appears.
4. Regression: a clean `exit 0` service and a `exit 1` service keep their current
   states.

## Background

- [`../../sshd/docs/EXIT_STATUS_FIX.md`](../../sshd/docs/EXIT_STATUS_FIX.md) —
  the same `waitpid` limitation seen from sshd, and where `WaitStatus` /
  `waitpid_status` came from.
- `src/syscall/proc.rs` — `encode_wait_status`, `sys_kill`.

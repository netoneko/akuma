# SIGUSR1/SIGUSR2 and the real-time signals did not terminate by default

2026-11-04. Found while bringing signals to Taoxichuan on aarch64 (B21 A5.7,
`docs/archive/TAOXICHUAN_B21_AARCH64.md`).

## Symptom

`sh -c 'kill -USR2 $$; echo no'` printed `no` and exited 0 on every Akuma
kernel. On Linux the shell dies of `SIGUSR2` (status 140) and prints nothing.
The same holds for `SIGUSR1`, `SIGSTKFLT` (16), `SIGIO` (29), `SIGPWR` (30) and
every real-time signal (32..=64): with no handler installed, they were dropped.

## Cause

`akuma_syscalls_glue::signal::signal_is_fatal_default` listed only
`1-9, 11, 13-15, 24-27, 31`. The omission was deliberate. Linux's table was
tried on 2026-06-22 (`docs/archive/AKUMA_SELF_HOSTING.md` §7k.5 item 2), and the
in-VM `rustc` self-host build then died with `signal: 10` at unit 146/147,
reproducibly, so the table was reverted. That entry names the real fix,
"`tkill`'s handler attribution", and leaves it for a future pass.

Reading `sys_tkill` again, it had two defects, and the second one is what made
`signal: 10` fatal to the *sender*:

1. **An unattributable target became `Default`.** When `find_pid_by_thread(tid)`
   or `lookup_process_shared(pid)` failed (a thread mid-creation or mid-teardown
   during a storm of about 10,400 `tkill(SIGUSR1)`), the handler was taken to be
   `SignalHandler::Default` rather than the target's real `UserFn`.
2. **The default action ran on the caller.** For `Default`, a fatal and
   unblocked signal called `sys_exit_group(-sig)`, which ends the **calling**
   thread's group, whoever `tid` named. `SIGKILL` did the same unconditionally.
   A `tkill` aimed at another process (or one whose lookup failed) killed the
   process that sent it.

With the conservative table, defect 2 was reachable only through the signals
already in it, so a stray `SIGUSR1` was dropped and nothing died.

## Fix

- `signal_is_fatal_default` is Linux's table: every signal in `1..=64` except
  the ones whose default is to ignore (17 `SIGCHLD`, 23 `SIGURG`, 28 `SIGWINCH`)
  or to stop or continue (18-22; Akuma has no job control, so a stop is a no-op).
- `sys_tkill` attributes or drops. A tid with no process, or a process already
  gone, has no disposition to run, so the signal is dropped (returns 0).
- A fatal default (and `SIGKILL`) ends the **target's** group. When the target
  is in the caller's own group, `sys_exit_group` runs there as before. Otherwise
  the signal is pended on the target and `interrupt_thread` wakes it from any
  park, and it dies at its own next return to EL0
  (`akuma-exceptions::apply_default_signal_action`).
- `akuma-syscalls-glue-amd64::kill::tkill` already pended everything and decided
  fatality at the target's delivery. It had the same `Default` fallback for an
  unattributable tid (which only raised a stray interrupt) and now drops
  instead, the same as glue.

Taoxichuan delivers through `akuma_tokio_kernel_core::signal`, which reads the
*current* process's own table at the target's syscall return, so no signal
reaches a default action there unattributed.

## A second defect found on the way: `rt_sigprocmask` answered `EINTR`

The amd64 battery at smp=4 then hung in `pidfdprobe`'s
"`pidfd_send_signal(SIGTERM)` kills a second child": the child slept forever and
the parent's `waitpid` never returned. The console showed the child's
`rt_sigprocmask` failing with `-EINTR`.

musl's `fork` blocks application signals around the syscall and the child
restores its mask with `rt_sigprocmask` on the way out. A signal sent in that
window pends (it is blocked) and raises the thread's interrupt bit. glue's
dispatch prologue answered **every** call with `EINTR` while that bit was set,
the mask restore included, so it never ran: `SIGTERM` stayed blocked and the
child went on to `sleep(100)` in a loop. This is the same shape as the musl
`raise`/`abort` failure the amd64 delivery path already works around by clearing
the bit at each return. The window is a few instructions, which is why the
battery passed for months.

Fix: `rt_sigprocmask`, `rt_sigaction`, `rt_sigpending`, `rt_sigreturn` and
`sigaltstack` never answer `EINTR` from the prologue, and do not consume the
bit. Linux never returns `EINTR` from any of them, and they are how a program
makes a pending signal deliverable.

## Verification

- Taoxichuan aarch64, TCG: `sh -c 'kill -USR2 $$; echo no'` dies with status
  140 and prints nothing; `sleep 5 & kill -USR1 $!; wait $!` reports 138; a
  `trap` handler for `USR1` still runs and returns.
- `test_sigchld_not_fatal_by_default` (the boot suite, `src/process_tests.rs`)
  now checks the real table: 17-23 and 28 do not kill; 9, 10, 12, 15, 34 and 64
  do; 0 and 65 are not signals. Before, it checked an inline copy of the old
  table.
- amd64 Taoxichuan probe battery (tokioprobe, killtree, pidfdprobe, `wait`) at
  smp=1 and smp=4: all pass, 0 faults, after both fixes. Before the
  `rt_sigprocmask` fix the smp=4 run hung in `pidfdprobe`.

## Correction (2026-11-05): the boot suite was not run, and one of its tests asserted the old rule

The verification above ran Taoxichuan and the amd64 probe battery, **not the main kernel's boot suite**.
That suite's `test_tkill_blocked_fatal_is_pended` (`src/pthread_tests.rs`) sent a blocked SIGTERM to a
kernel thread from `threading::spawn_fn` and asserted it was pended. A kernel thread has no owning
process, so under this fix's rule the signal is dropped, and the main kernel panicked at boot on every run
(`a blocked fatal signal was not pended for later delivery`). Found 2026-11-05 booting the main kernel at
SMP=4 under HVF to run Infinity on it. It reproduced 3 of 3, including on a kernel built before B21 A6. The
`[BKL] stuck owner=1 tag=511` lines printed just before it are the known load-driven storm and are
unrelated: an instrumented build showed `tkill` returning in 3 us with the target's mask still set and
nothing pended.

The rule stands, and the test now asserts it: `test_tkill_to_a_kernel_thread_is_dropped` checks that a
fatal signal to a kernel thread, blocked or not, returns 0, pends nothing and leaves the thread alone.
The "blocked fatal default -> pend" branch is reachable only with a real process target now, so its
coverage moved to the host: `sys_tkill`'s decision is `akuma_syscalls_signal::tkill_action`, a pure
function of (signal, target disposition and group, or no process, mask), and glue only looks up and acts.
Its tests are per-branch cases, an exhaustive sweep of every signal x target x blocked state checked
against stated invariants (no process -> drop; nothing reaches the caller's group unless the target is in
it; a blocked signal never acts now; only an unblocked fatal default or SIGKILL kills; a handler always
pends), and a seeded 200,000-case fuzz showing that only the signal's own mask bit matters.
`signal_is_fatal_default` forwards to the same crate's `fatal_by_default`, so the table has one copy.

Verified: the boot suite passes at SMP=4 (HVF, 2 GiB) and the kernel serves ssh; `cargo test -p
akuma-syscalls-signal` passes (17 tests).

## Still open

- **Not yet run: the in-VM `rustc` self-host build (`-j4`)**, the workload that
  failed on 2026-06-22. It is the gate for this change. If it dies with
  `signal: 10`, the storm reaches a default action by a path other than
  `sys_tkill`; look first at a thread receiving `SIGUSR1` after its process's
  table was reset by `execve`.
- `sys_kill` (`kill(2)`) pends to a group and decides fatality at delivery, so
  it was not affected. It was not re-read for the new signals beyond that.
- Real-time signals are still coalesced, not queued (`[GAP ]` at boot).

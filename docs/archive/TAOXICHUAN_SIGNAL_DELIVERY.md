# Taoxichuan delivers signals: `kill`, handlers at every syscall return, fatal signals on the tick (2026-11-01)

**Stability: B**: verified on the Mac TCG rig at `smp=1` and `smp=4`; not yet on the ryzen.

Cleanup step B9 (`TAOXICHUAN_CLEANUP_PLAN.md`). Before it, Taoxichuan delivered exactly one thing: SIGCHLD to a handler,
inside `rt_sigsuspend`. `kill(62)` was `ENOSYS` (tokio's `Child::kill` failed), `tkill` only interrupted a blocking call,
and no other handler ever ran.

## What it does now

- **`kill`, `tkill`, `tgkill`** are the x86 kernels' shared bodies, moved from `amd64/src/signal.rs` into
  `akuma-syscalls-glue-amd64::kill` (amd64 forwards to them; its `sys_kill` history stays in the doc comments). A signal
  is *pended* on the target (`akuma_exec::process::pend_signal_to_group`, or the one thread for `tkill`) and taken by each
  thread at its own next return to ring 3. Nothing marks a running thread dead from another core.
- **Delivery at every syscall return** (`child::deliver_at_return`, from the dispatcher): the lowest pending signal the
  mask lets through. `SIG_IGN` drops it; a default action that kills ends the group (`task_exit_code(-sig)`, so
  `WIFSIGNALED`); other defaults (CHLD, CONT, URG, WINCH, the stop signals: no job control) are dropped; a handler gets a
  frame. One handler per return; the next pending signal waits for the `rt_sigreturn`, itself a syscall return.
- **The frame** (private to `push_handler`/`sigreturn_arm`): the saved registers, the user `rsp`, the mask to restore and
  the FP image's address, then a zeroed `ucontext`, a `siginfo` (`si_signo`, `SI_USER`) for `SA_SIGINFO` handlers, and
  the FP/SSE image (64-byte aligned). The handler starts with the FP state reset, as on Linux; `rt_sigreturn` sanitizes
  the image a program hands back before restoring it (four malformed shapes make `xrstor` a ring-0 #GP).
- **`SA_RESTART`**: a call that returned `EINTR` is re-run after the handler (saved `rip` back over the 2-byte `syscall`,
  `rax` its number), except the calls Linux never restarts (the sleeps and the timed waits).
- **`rt_sigsuspend`** wakes for any signal its temporary mask lets through, not only SIGCHLD, and delivers it under that
  mask.
- **The ring-3 tick** ends a thread looping without syscalls when a fatal signal is pending (`SIGKILL`, or a
  default-terminate with no handler). Handlers wait for the next syscall return.
- **`exit` reparents** the group's children (its threads' too) to init, which reaps them (`wait4(-1, WNOHANG)` in sshd).

## Bugs found on the way (each was invisible until handlers ran)

1. **The dispatcher undid the handler's stack.** It saves the user-`rsp` stash at entry and restores it at exit (a park
   inside a call lets other threads overwrite it), which also undid the `rsp` a signal frame or `rt_sigreturn` set: the
   handler ran on the interrupted stack and its `ret` never reached the restorer, so the signal's bit stayed in the mask
   and the second `raise` never delivered (`sigprobe` rung 2). Now a per-thread "return with this `rsp`" override wins,
   and the stash is restored *before* delivery, which builds on it.
2. **The interrupt flag outlived its purpose.** `kill`/`tkill` set a thread's interrupt flag so a *blocking* call returns
   `EINTR`; set on the caller itself (`raise`, `abort`, `kill(getpid())`), it made the caller's next glue call answer
   `EINTR` without running: musl's `raise` lost its mask restore, so the signal stayed blocked (`sigprobe` rungs 1 and 8;
   amd64 hit the same trap and documented it). The flag is now consumed at every return to ring 3, where nothing blocks.
3. **A `dup` lost `O_NONBLOCK`**, and tokio's signal driver, which drains a dup of its non-blocking socketpair until
   `EAGAIN`, parked forever on the second read: the runtime stopped, timers included
   ([`DUP_LOST_O_NONBLOCK.md`](DUP_LOST_O_NONBLOCK.md); every glue kernel had it).
4. **Orphans leaked their thread slots.** Taoxichuan's exit never reparented children, so a killed tree's grandchildren
   stayed registered under a dead parent; `killtree` exhausted the 48-slot ring by its third round.

## Verified (Mac TCG rig)

- `sigprobe` (`userspace/forktest/c_stress`, musl static): rungs 1-8 (raise, resume, siginfo, mask, nested, eintr,
  fatal, abort).
- `killtree 8` at `smp=1` and `smp=4`: PASS, every mode (SIGKILL the browser, the group, SIGTERM then SIGKILL), tree reaped
  in 26-320 ms, 0 bystander errors. Before B9 the probe could not run (`kill` was `ENOSYS`).
- Shell: `trap ... USR1` + `kill -USR1 $$` runs the trap; `kill` of a sleeper reports 143, `kill -9` 137, and a spinning
  `sh` loop dies by SIGTERM through the tick (143).
- tokioprobe with 4 workers at `smp=1` and `smp=4`; battery + goose, toolchain gate, `cow_leak_gate`: see the B9 row.

## Not done (known, pinned)

- **Faults and the tick do not run handlers.** A CPU fault kills with SIGSEGV directly (`sigprobe` rungs 9, 10), and a
  handled signal to a thread that never makes a syscall waits (rung 12). Both need a frame built from an interrupt frame and
  a return through `iretq`; tokio does not need either (Rust's std SIGSEGV handler only prints "stack overflow").
- **`SA_ONSTACK` is ignored**: handlers run on the interrupted stack.
- **A group signal can run its handler on every thread** (`pend_signal_to_group` pends on each), as on amd64. Linux picks
  one.
- **`PR_SET_CHILD_SUBREAPER` is accepted but not honoured**: orphans go to init.
- The `ucontext` handed to `SA_SIGINFO` handlers is zeros; a handler that edits `uc_mcontext` changes nothing.

## Background

- [`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md): step B9.
- [`AKUMA_AMD64_SIGKILL_NATIVE_PATH.md`](AKUMA_AMD64_SIGKILL_NATIVE_PATH.md): why `kill` pends instead of hard-killing.
- [`TAOXICHUAN_EXIT_GROUP_RUNNING_SIBLING.md`](TAOXICHUAN_EXIT_GROUP_RUNNING_SIBLING.md): the same rule for `exit_group`.

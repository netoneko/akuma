# amd64 boot self-test: the thread probe sometimes reports exit status 0 (2026-11-01, open)

**Stability: C**: open, logged as a flake by decision. Not root-caused.

## Symptom

`amd64/run.sh` (QEMU TCG, `SMP=1`), the boot self-tests, `thread_test` (`amd64/src/usermode.rs`):

```
  thread: probe loaded   [OK]
  thread: probe spawned   [OK]
[threads] new high-water: 3 live user threads (terminated=0 free=509 ceiling=512)
  thread: probe reported every check   [FAIL] got 0x0 want 0x1ff
  thread:   clone returned a new tid   [FAIL]
  ... (every bit)
  thread: no thread outlived the process   [OK]
  thread: teardown leaks nothing   [OK]
```

Every other self-test passes, including `kill`'s thread probe right after it. No fault line, no panic.

## Rate

| tree | runs | thread-probe failures |
|---|---:|---:|
| `e2d3bdc1` (before A5) | 5 | 0 |
| A5 (`akuma-sched-amd64::hooks::<P>()`), uncommitted | 15 | 3 |
| A5 with `safe_print!` on every exit path (below) | 6 | 0 |

0/5 against 3/15 does not prove A5 introduced it (at a 20% rate, five clean runs happen a third of the time), but A5 is
the likelier side. The instrumented runs stopped reproducing it, which is what a timing-dependent race does.

## What is known

- **The status is not the probe's own.** `userspace/amd64/threadprobe/threadprobe.rs` ORs bit 8 (`mmap` worked) into its
  status unconditionally before its `exit_group`, and that `mmap` runs before the `clone`. A `0` therefore never came
  from the main thread's `exit_group`.
- **What can store `0` into `EXIT_STATUS`:** the `Syscall::Exit` arm when `exit::scope` answers `Group`, which needs
  `crate::thread::current_is_main()` to be true for the caller. The child thread ends with `exit(0)` (`mov rax, 60; xor
  edi, edi`). So the likeliest story is a child thread whose `exit` was judged a main-thread exit:
  `current_thread_slot()` read `NO_THREAD` from the running task's `UserCtx`. The other writers (`unwind_to_kernel`,
  `run_process`'s `enter_user` return) would need a `0` status from a fault or a return.
- **A5 changes no amd64 behaviour on paper.** The hook table `hooks::<Amd64>()` builds holds the same functions the
  hand-written table did (`current_task` -> `smp::current_task`, `set_current_task`, `idle_task`, `allow_tick`,
  `write_user_context`/`read_user_context`, `prepare_task_slot` -> the crate's generic), now reached through
  monomorphized trait methods. Code layout and inlining differ, which moves timing.
- `SMP=1`, so not a cross-core race. Under TCG the timer tick lands at different instructions on every run.

## Decision

Logged as a flake and A5 committed (user's call, 2026-11-01): chasing it now would block the process-model work, and
the next occurrence can be caught with the instrumentation below.

## Next time it appears

Re-add, in `amd64/src/usermode.rs`, one `safe_print!` each in the `Syscall::Exit` arm (`a1`, `current_proc_slot()`,
`crate::thread::current_thread_slot()`, `crate::sched::current_task()`, the scope), the `Syscall::ExitGroup` arm, at
the top of `unwind_to_kernel` and after `run_process`'s `enter_user`. Then loop `amd64/run.sh` until a failure and keep that
log. Expect to need more runs than without the prints. The question to answer is whether a child thread's `exit` saw
`thread_slot == NO_THREAD`, and if so, which write to that `UserCtx` (the clone path, `prepare_task_slot`'s reset of a
recycled slot's user context, or the switch publishing `current_uctx`) it raced.

## Background

- [`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md): step A5.
- [`AKUMA_AMD64_RUST_STD.md`](AKUMA_AMD64_RUST_STD.md): where `thread_test` and the probe come from.

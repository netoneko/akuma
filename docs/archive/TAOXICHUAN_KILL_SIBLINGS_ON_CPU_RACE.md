# killtree at smp=4: `exit_group` ended threads that were still running

2026-11-04. Taoxichuan (both machines share the code: `akuma-tokio-kernel-core::proc`).

## Symptom

`killtree 8` (`userspace/forktest/c_stress/killtree.c`) at smp=4 on amd64
Taoxichuan faulted in about half the boots:

- worker processes died with `[Fault] ring3 PageFault rip=0x40215c ... code=0x4`;
- killtree's own main jumped to 0 (`rip=0x0 code=0x14`);
- a **fresh** killtree died at its first instruction (`rip=0x400cc7`, `_start`).

The instructions at those addresses (`jmp __libc_malloc_impl`,
`xor %rbp,%rbp`) cannot raise a data fault. Measured on the A5.6c commit
(`fe59580a`) and on A5.7, running killtree twice per boot: faults in 4/9 and 5/9
boots. A5.5 had one too. It was older than the B21 work.

## How it was found

1. The fault report read `CR2` live after `lazy::fill`, which can park on a disk
   read, so the address it printed belonged to whatever last faulted on that
   core (`0`, `0x8c`). It is captured once at entry now.
2. A trace-err dump of the bytes mapped at the faulting `rip`
   (`fault_code_dump`) showed killtree's text page mapping a frame with CoW
   count 2 (one mapper, the file-page cache) holding **zeros** or **foreign
   code** (`8b 86 8c 00 00 00`: `mov 0x8c(%rsi),%eax`). So a frame the cache still
   held had been freed and reused, and something then wrote into it.
3. `exit_group` (`kill_siblings`) decided whether a sibling was executing from
   its **state word**.

## Cause

`kill_siblings` ended in place any sibling whose state was not `RUNNING`
(`mark_thread_terminated`, a plain store), and asked only `RUNNING` ones to end
themselves. But on the hooked scheduler a thread is executing when its `ON_CPU`
gate is held, and the state word does not say that:

- `x86_pick_next` on another core reads `READY`, wins the `ON_CPU` claim, and
  switches the thread in. A `TERMINATED` stored in between does not stop it.
- The leader was never in the loop at all: `task_exit_code`, run by a non-leader
  thread (killtree's `kill(<worker tid>, SIGKILL)` mode), marked the leader
  terminated unconditionally, wherever it was running.

The exiting thread then made the root a zombie and the reap freed the page
tables and every frame, while the "terminated" thread ran on in user mode on
another core under that root. Its stores landed in frames the PMM had handed
out again, including file-cache text pages that the next killtree mapped.

## Fix

- `akuma_threading::x86_hold_off_cpu(slot)`: a CAS of `ON_CPU` from 0 to 1. While
  held, the thread is not executing and no core can pick it.
- `kill_siblings` stops every other thread of the group, the leader included. A
  thread whose gate it can hold is ended in place under the gate (marked
  `TERMINATED`, then the gate is released; `x86_pick_next` skips a
  `TERMINATED` slot). One it cannot hold gets a kill request and is re-tried until
  it ends itself (`finish_killed`) or comes off its core. A thread still on a core
  after 2 s is reported and waited for, never ended where it runs.
- The leader keeps its slot and `Process` when stopped (in place, or by
  `finish_killed` recognising it is the leader); the exiting thread publishes
  the status there. The zombie hold on the leader's slot is taken *before* the
  waits, which release the kernel lock.

## Verification

- killtree twice per boot at smp=4, 6 boots: 12/12 PASS, 0 faults. Before: 13
  of 21 boots faulted (baseline and A5.7 together; the dump kernel 3/3).
- amd64 probe battery at smp=1 and smp=4: tokioprobe, killtree, pidfdprobe, `wait`
  all PASS, 0 faults.
- aarch64 TCG: busybox fork/exec, pipe, `USR1`/`USR2` checks and 450 fork+execs, as before.

## Still open

- `[TRAMP-MISMATCH]` (a zombie's `thread_id` matching a reissued tid) is still
  printed about 17 times per run. Its detector calls it benign, because
  `THREAD_PID_MAP` is authoritative. Not investigated further here.
- `pmm-forensics` (new feature, amd64 Taoxichuan) turns on the PMM's
  quarantine, premature-free check and CoW ledger. It slows TCG enough that the
  rig's 400 s run limit can cut a killtree run short.

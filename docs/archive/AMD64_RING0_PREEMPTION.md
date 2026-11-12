# amd64 ring-0 preemption, outside the BKL (`ring0-preempt`)

Date: 2026-11-11. Branch `akuma-multiarch-attempt-0`. Part of the scheduler unification, after
AArch64's hooked path took EL1 preemption (`docs/archive/HOOKED_EL1_PREEMPTION.md`).

## Where amd64 stood

The timer switched only ring 3 and the idle loop (`sched::preempt_if_needed`). The module header
gave the reason as a heap spinlock left held by a preempted kernel thread, but that reason is out
of date: `akuma-alloc` masks interrupts on x86. The real constraint is simpler: **ring 0 almost never
ran with interrupts enabled.**

- `IA32_FMASK` clears `IF` on `syscall`, and nothing set it again, so every syscall ran masked
  from entry to `sysretq`.
- Kernel threads start with `IF` set (`akuma-switch`'s `0x202`). The x86 `wfi`/`irq_window`
  brackets end in `cli`, so a thread that ever halted ran masked from then on.
- So a tick could not land in kernel code. The "no ring-0 preemption" rule was there to keep
  a thread from losing its place in the middle of kernel code.

## The BKL question

On amd64 the BKL is `akuma_bkl`'s `KernelLock`. It is reentrant by owner core and has no depth,
and it travels with the core across a switch. A syscall takes it at entry (`usermode.rs`) and
holds it throughout, except in the shared crates' dropped-BKL windows (`no-bkl-vfs/network/
process/mm/drivers`, all forwarded by amd64's `smp-shared`).

AArch64 hooked preempts EL1 **with** the lock held. The next thread on that core then runs
inside the preempted thread's section. The shared crates are built for that. amd64's own kernel
code (`usermode.rs`, `signal.rs`, `thread.rs`, the per-core `MACHINE` writes in `sched.rs`) was
written for a ring 0 that never switches involuntarily, and has not been audited for it.

## What changed (feature `ring0-preempt`, off by default)

1. **The gate.** `sched::ring0_preemptible()` is `!smp::bkl_held() &&
   threading::el1_preemptible()`, the AArch64 gate plus "this core does not hold the BKL".
   - `el1_preemptible` covers spinlocks held under `PreemptGuard` (preemption disabled), the
     scheduler pool lock, and a thread half-way into a park (not `RUNNING`).
   - The switch goes through `sched::yield_now`, which takes the BKL for the switch and gives it
     back on resume. That is the rule the 2026-09-26 metal hangs established: never switch
     without the lock.
2. **Syscalls run with `IF` set** between the BKL entry and the epilogue. The epilogue runs
   masked from the signal check to `sysretq`.
3. **The shared switch masks interrupts on both architectures.** `x86_yield_now`'s
   `irq_save_mask`/`irq_restore` were AArch64-only. On x86 they are `pushfq; cli` .. `popfq`, a
   no-op for masked callers.
4. **`allow_tick` masks its own bracket and restores the caller's `IF`.** Otherwise a tick could
   switch a caller between dropping the BKL and the halt. The trailing `cli` also no longer leaves
   a preemptible caller masked.

Counter: `sched::ring0_preemptions()`, on the netprobe status line as `r0pre=`.

## Verification

QEMU TCG, `-M microvm`, `INITARGS=uname,-a`.

New boot check `sched::ring0_preempt_smoke_test` (feature builds only): a kernel thread spins with
the BKL dropped and `IF` set, never yielding, until a flag that only a second thread sets. On one
core it can only finish if the tick switches it out.

| build | SMP | self-test | ring-0 check |
|---|---|---|---|
| default | 1 | 829/0 | not built |
| `ring0-preempt` | 1 | 831/0 | OK, 2 preemptions |
| `ring0-preempt`, gate forced `false` (negative control) | 1 | 829/2 FAILED | both checks fail, 0 preemptions |
| default | 2 | 839/0 | not built |
| `ring0-preempt` | 2 | 841/0 | OK |
| default | 4 | 839/0 | not built |
| `ring0-preempt` | 4 | 841/0 | OK |

Every run reached init, and `uname -a` answered. SMP=4 under TCG finished both times here,
although the multiarch log records it as a flake. The real SMP gates still need KVM.

## Open

- **BKL-held preemption.** This is what AArch64 does. It needs an audit of amd64's own BKL
  sections for per-thread exclusion and for per-core state read across a switch. Until then the
  two gates differ by the BKL term.
- KVM gates (Firecracker on the ryzen, the metal) before the feature becomes the default.

## Background

- `docs/archive/HOOKED_EL1_PREEMPTION.md`: the AArch64 half.
- `docs/archive/AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md`: why no switch may run without the BKL.
- `docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.30: the spinlock deadlock that "preemption switched
  into", which `el1_preemptible`'s preemption-disabled term prevents.

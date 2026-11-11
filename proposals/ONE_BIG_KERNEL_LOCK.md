# One big kernel lock for Akuma and Taoxichuan (proposal, 2026-11-11)

**Status: proposal, not started.** Deferred to the big merge on purpose: the merge decides the
lock anyway, and this is the riskiest part of it.

## Where things are

Two kernels' worth of SMP run on two different big kernel locks, over the same shared crates.

| | Akuma (AArch64 + amd64) | Taoxichuan (`akuma-tokio-kernel`) |
|---|---|---|
| lock | `akuma-bkl` (`KernelLock` in `sync.rs`, the wrappers in `bkl.rs`, the drop policy in `policy.rs`) | `akuma_tokio_kernel_core::bkl` (241 lines) |
| shape | recursive, hold-tag attribution, **dropped windows**: exec's ELF read, demand-page fill, VFS, mm and drivers leave the lock and re-enter it, with a per-thread ledger so a tick cannot re-take it mid-window | ticket lock, re-entrant by owner core, depth per thread handed over at a switch (`transfer_depth`); taken around every kernel entry, **no dropped windows** |
| machine hooks | the kernel's exception path calls `enter_kernel`/`leave_kernel`/`reconcile_for_spsr` | `BklCpu`: `index`, `while_waiting` (amd64 services TLB shootdowns while spinning), `irq_save`/`irq_restore` |
| how the shared crates see it | built with `bkl-smp` (`cfg(kernel_bkl_smp)`): they call `akuma-bkl` themselves, open dropped windows, mask IRQs in `PreemptGuard` | built **without** `bkl-smp`: every `akuma-bkl` call is an empty body and the embedder serializes around them |

The `bkl-smp` switch on the 16 shared crates exists only because of this split. Both Akuma kernels
always turn it on; Taoxichuan never does; host tests use the off arms.

## Goal

One lock. Taoxichuan takes `akuma-bkl`, the switch becomes unconditional, and the off arms survive
only as `cfg(test)` (or go, where a test does not need them).

## Why it is not mechanical

- **Dropped windows are new behaviour for Taoxichuan.** Today its kernel code is never preempted
  out from under its lock in the middle of an exec read or a page fill. Turning the windows on
  means the races `docs/archive/` records for Akuma's BKL work (the dropped-window ledger, the
  `ON_CPU` gate, the TTBR gate) become reachable in Taoxichuan. The windows are runtime toggles in
  `akuma_bkl::policy`, so a first step can run with all of them off.
- **The hooks differ.** Taoxichuan's amd64 machine services TLB shootdowns while it waits;
  `akuma-bkl`'s wait would need the same callout (the `BklCpu::while_waiting` shape) or an amd64
  Taoxichuan core spinning IRQ-masked never takes the shootdown IPI.
- **Depth hand-over.** Taoxichuan moves the lock depth with the thread at a switch
  (`transfer_depth`, `born_locked`); `akuma-bkl` reconciles on the way back to EL0
  (`reconcile_for_spsr`). The two must be mapped onto each other, per architecture.
- **Fairness.** Taoxichuan's lock is a ticket lock because an unfair spin starved sshd under four
  compilers; check `KernelLock`'s order before swapping.

## Steps, each gated on its own

| step | what | gate |
|---|---|---|
| 1 | Read both locks side by side and write the mapping (enter/leave/nesting/hand-over/IRQ masking/waiting callout) into this file | review |
| 2 | Give `akuma-bkl` a `while_waiting` callout; Akuma kernels pass a no-op on AArch64 and their shootdown service on amd64 | Akuma boot suites, SMP=4, unchanged images where the callout is a no-op |
| 3 | Taoxichuan on `akuma-bkl` with every dropped-window toggle **off**: same serialization as today, one lock implementation | Taoxichuan SMP=4 on both arches, its self-host build |
| 4 | Turn `bkl-smp` on for Taoxichuan's crates; delete `akuma_tokio_kernel_core::bkl` | as 3, plus `cargo tree` shows one lock |
| 5 | Enable dropped windows one toggle at a time, A/B/A, as Akuma's BKL phases were | per toggle |
| 6 | Make `bkl-smp` unconditional; host tests keep the off arms under `cfg(test)` | full matrix |

## Background

- `docs/archive/AKUMA_FEATURE_AUDIT.md` — why `smp-shared` became `bkl-smp` and could not simply be
  removed.
- `docs/reference/subsystems/smp-shared.md`, `docs/reference/subsystems/locking.md` — Akuma's BKL.
- `crates/akuma-tokio-kernel-core/src/bkl.rs` — Taoxichuan's, with its own reasoning at the top.

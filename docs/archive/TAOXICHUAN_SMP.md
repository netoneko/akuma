# Taoxichuan SMP: one big kernel lock, N cores (2026-10-31)

**Stability: C** - written as the work landed; the measurements at the bottom are the evidence.

Before this the Taoxichuan kernel ran on one core of the four vCPUs it was given. `smp=N` now starts N-1 application
processors that run user tasks in parallel under one big kernel lock. Design and rules:
`taoxichuan/docs/reference/scheduling.md` § SMP, figure `taoxichuan/docs/diagrams/smp.md`.

## Stages (each a commit, each green before the next)
| stage | change | gate |
|---|---|---|
| S-a | per-CPU block behind `%gs` (`akuma-ring3-amd64::PerCpu`): the syscall stub `swapgs`es and reads its stack top and user-rsp stash from `gs:[16]`/`gs:[8]`; `enter_user`/`child_enter` `swapgs` on the way to ring 3 | `smp=1` and `smp=4` (APs not yet started) 9/9 |
| S-b | MADT, INIT/SIPI with the `boot.s` trampoline, per-CPU GDT/TSS/trap stack, per-core IDT/SSE/syscall MSRs; APs halt | `smp=2/4/8` boot, `cpus online` |
| S-c1 | running slot and dispatcher task record per CPU; an idle thread per core | 6/6 |
| S-c2 | the big kernel lock (`smp.rs`): syscall landing, ring-3 faults, ticks; depth per thread through `transfer_lock_depth` | `smp=1` 9/9, `smp=4` 6/6 |
| S-c3 | APs adopt an idle thread, tick, and join the picker | `smp=1/2/4` 12/12, 12/12, 32/32 |
| S-d1 | TLB shootdown (vector 33) after a CoW copy, an unmap, an `mprotect`, a fork | `smp=4` 24/24 |

## Bugs the bring-up found (each was a hang or a triple fault, none visible without two real cores)
1. **A stale CR3 under a reaping peer** (triple fault, `switch_to`+0x16c). `switch_to` left CR3 alone for an idle thread, which is harmless on one
   core. With two, an idle core sat on a finished task's root while the other core reaped the task and freed its page tables. Fix: an idle thread loads
   the kernel root.
2. **The pick and switch ran without the lock.** An idle core called `x86_yield` lock-free; the `switch_to` hook reads `SLOT_STATE`/`TASKS`, which the
   other core was mutating under the lock. Symptom: `[SWITCH BADFRAME]` and `InvalidOpcode rip=1`. Fix: the idle lap takes the lock for pick + switch.
3. **The boot core's idle thread was not pinned.** `can_run` pinned only idle slots above `NSLOTS`; the BSP's is `NSLOTS` itself, so an AP picked it
   and ran on the BSP's idle stack. Symptom: a saved switch frame overwritten with an interrupt frame. Fix: every idle slot `>= NSLOTS` is pinned.
4. **`page-table symbol addresses are physical`**: the AP boot root is `__ap_pml4`, linked VMA = LMA; subtracting `KERNEL_VMA` wrapped and the AP
   triple-faulted before printing anything.

## Not caused by SMP (found on the way, still open)
* `busybox sh`'s `wait` calls `rt_sigsuspend` (nr 130), which answers `ENOSYS` in a loop, so a script that backgrounds jobs and `wait`s hangs about half
  the time - identical on the pre-SMP kernel (3 of 7 vs 5 of 8 probes). The gates use foreground forks only.
* The epoll edge-drain hooks sat inside the `trace-err` block, so a plain kernel neither built (`-D dead-code`) nor cleared an edge (`72585f1d`).

## How it was verified
Ryzen under Pop!_OS, Linux KVM, `qemu-system-x86_64 -M microvm`, `taoxichuan_pop.sh` with `TX_SMP=N`; kernels built on the Mac. The gate is a fork/pipe probe
over ssh repeated three or four times per boot on several boots; a triple fault was diagnosed from QEMU's monitor (`-no-shutdown`, `info registers -a`),
because `-d int` does not exist under KVM.

## Measurements
In-guest `cargo build --release -p taoxichuan -j4` (Linux KVM guest on the ryzen, same kernel `kb-fix` = `5108b5e5`):

| CPUs | wall | result | artifact md5 |
|---|---|---|---|
| 1 | 7m14s | EXIT=0, 0 console faults | `38852d540d8af420ac894544916cc2a1` |
| 4 | 13m11s | built (the `EXIT=255` is the ssh client, not cargo), 30 fault-ish console lines | `38852d540d8af420ac894544916cc2a1` |

Same bytes on 1 and 4 CPUs: the build is deterministic. **Four CPUs are 1.8x slower than one** - the big kernel lock is the bottleneck (the 4-CPU run
logged 20 "stuck" lock reports), so SMP here buys correctness of the parallel ring-3 path, not speed yet. The next step is finer-grained locking.

Metal (ryzen, Akuma host `5108b5e5b` on entry 29 = the "entry 30" label, `smp=8`, `kvmctl kvmmem=4096`): the host kernel built on the ryzen with the shared
`akuma-sched-amd64` boots, joins wifi, enables SVM on cores 1-7. Taoxichuan as a 4-vCPU guest (`vm_vcpus = 4`, `smp=4` on the guest cmdline): the first
boot stopped on a nested page fault at `0xfee00020` - Akuma's SVM model starts a non-boot vCPU with `IA32_APIC_BASE` enable clear for a plain guest, and
`ap_entry64` read the APIC ID before enabling it. Fixed by setting the bit in `ap_entry64`; then `smp: 4 cpus online`, ssh round trip OK, state running.

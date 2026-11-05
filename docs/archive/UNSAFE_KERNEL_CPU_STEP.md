# Unsafe audit steps 1-2: the zero-cost removals and the `akuma-cpu` named functions

**Date:** 2026-11-05. **Plan:** [`UNSAFE_KERNEL_AUDIT.md`](UNSAFE_KERNEL_AUDIT.md) steps 1 and 2; precedent
[`AMD64_UNSAFE_REDUCTION_TRAP_MSR.md`](AMD64_UNSAFE_REDUCTION_TRAP_MSR.md). **Written by:** goose-glm (`glm-5.3`, on the
ryzen box under Pop OS) up to its quota, then reviewed, corrected in one place, verified and merged by the supervisor.
Steps 3-7 of the audit (PhysSlice, SVM, AP bring-up, ring3, per-CPU GDT, LAPIC, the PTE walker) are untouched.

## What moved

| step | old site | now |
|---|---|---|
| 1 | `tokio-kernel-amd64` `trap_top_for`; `amd64/src/gdt.rs` `TRAP_STACK` end | no `unsafe`: address arithmetic on a static (`wrapping_add`), no dereference |
| 2 | `amd64/src/smp.rs` `enable_sse`; `tokio-kernel-amd64` `enable_cpu_features` | `akuma_cpu::ctlreg::enable_fpu()` (CR0.EM=0, CR0.MP=1, CR4.OSFXSR, CR4.OSXMMEXCPT) |
| 2 | `smp.rs` `enable_xsave` | `ctlreg::enable_xsave(xcr0)` (CR4.OSXSAVE, then `xsetbv`; the CPUID facts are the caller's, stated in the doc) |
| 2 | `uaccess.rs` `init_smap`; `tokio-kernel-amd64`'s two CR0.WP writes | `ctlreg::enable_smep_smap(smep, smap)` and `ctlreg::set_cr0_wp()` |
| 2 | `sti`/`cli` in `lapic.rs`, `boot.rs`, `clock.rs`, `sched.rs` (selftests) | `interrupts::{on, off}` |
| 2 | `main.rs` and `tokio-kernel-amd64` halt loops | `park::halt() -> !` (`cli; hlt` per iteration, so an NMI wake re-halts) |
| 2 | byte-identical `rdrand` loops in `amd64/src/net.rs` and `tokio-kernel-amd64/src/net.rs` | `rng::rdrand() -> Option<u64>` (ten retries; the CPUID gate stays the caller's) |
| 2 | `multiboot2.rs` PAT write | `msr::set_pat_entry_wc(index)` (one entry becomes WC, the other seven written back as read) |

There is no raw `write_crN` anywhere in the public surface: each writer read-modify-writes exactly its own bits, the closed-set
design `msr` and `akuma-port` already use.

## What was reviewed and changed

Goose also added `msr::set_kernel_gs_base(base)`, a *safe* function behind a canonical check, for the one `IA32_GS_BASE` write in
`smp.rs::install_percpu`. **That was reversed.** `GS_BASE` is the per-CPU base that every per-CPU read dereferences, so a canonical
but wrong value hands safe code a wrong pointer; it is the same class as `TPIDR_EL1`/`TPIDRRO_EL0`, whose writes the crate
deliberately keeps `unsafe` ("writes to either re-point state the kernel *dereferences*"). The canonical check discharges the `#GP`
on the `wrmsr`, not the pointer. The write is `unsafe { msr::write(Msr::GsBase, addr) }` again, at its one call site, with the
reason in the SAFETY comment, and `crates/akuma-cpu` and `CLAUDE.md` say so. Cost: one site.

Judgement calls left as written, because they cannot hand safe code a pointer, and recorded here in case that changes:
`interrupts::on` (safe, as `daif::unmask_irq` was; enabling interrupts before the IDT is loaded is a caller boot-order fact),
`enable_xsave` and `rdrand` (a violated CPUID precondition faults ring 0; it does not corrupt memory), and `set_pat_entry_wc` (a
live mapping selecting the edited entry changes memory type: a coherency hazard).

## Evidence

* `unsafe` sites (`scripts/check_unsafe_budget.py`, production): `amd64/src` 80 -> 69, `akuma-tokio-kernel-amd64` 29 -> 24,
  `akuma-cpu` 65 -> 73 (the named functions). `docs/reference/unsafe-budget.json` carries the new numbers; the ratchet moved down
  by 16 in the kernels for 8 added inside the one crate whose job is to hold them.
* Builds: `akuma-amd64`, `akuma-tokio-kernel-amd64` (with and without `middleware`), `akuma` and
  `akuma-tokio-kernel-aarch64` check clean; clippy with `-D warnings` on `akuma-cpu` and `akuma-amd64`; `akuma-cpu` host tests (7).
* Boot, amd64 under QEMU TCG (no KVM on the Mac), `SMP=4 sh amd64/run.sh`, 200-240 s: 832 `[OK]` lines, 0 `[FAIL`, 4 CPUs
  online and every secondary taking timer interrupts, `smap`/`smep` follow CPUID, `wp: CR0.WP is set`. The boot was cut by
  `timeout` while the netpoll daemon was spawning, not by a failure. Not exercised: the PAT/write-combining path (needs a
  framebuffer).
* `sh shanghai/mw_gate.sh all` (both machines, SMP=1): 186 `^PASS`, 0 FAIL, the same as before the change. This boots the
  tokio-kernel amd64 and aarch64 kernels, the callers of `enable_fpu` and `park::halt`.

## Not done

* KVM boot of the real kernel, and the metal ryzen: the review boots are TCG.
* `akuma-tokio-kernel-core` fails `clippy -D warnings` on `loader.rs:97` (a long first doc paragraph, from the ETXTBSY change). It
  is not part of this change and was left alone.

## Background

Brief given to goose: `scripts/kvm/goose/` session `20261105_1` ("Akuma unsafe-kernel reduction"); it ended mid-edit of
`CLAUDE.md`, which was recovered from its working tree.

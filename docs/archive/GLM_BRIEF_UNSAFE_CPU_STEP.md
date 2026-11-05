# Implement docs/archive/UNSAFE_KERNEL_AUDIT.md — step 1+2 (zero-cost removals, akuma-cpu fns)

You are working in /root/akuma (the Akuma bare-metal Rust OS repo). Read these first:
- docs/archive/UNSAFE_KERNEL_AUDIT.md   (the plan: ranked steps 1–7; you do only steps 1 and 2)
- CLAUDE.md                             (repo rules: isolate unsafe in leaf crates, one contract per crate; every unsafe has a SAFETY comment; scripts/check_unsafe_budget.py is the gate)
- docs/archive/AMD64_UNSAFE_REDUCTION_TRAP_MSR.md (the precedent shape for this work)

## Setup (do first)

```
cd /root/akuma
git checkout post-release-improvements
git pull origin post-release-improvements
git checkout -b unsafe-kernel-reduction
```

Work ONLY on this branch. Commit incrementally with real messages.

## Step 1 — zero-cost removals (2 sites)

The unsafe block is not needed at all in either; they contain only address
arithmetic (`&raw const` + cast + `add` + `as u64`), which is safe:
1. crates/akuma-tokio-kernel-amd64/src/lib.rs:170  `trap_top_for`
2. amd64/src/gdt.rs:100  `TRAP_STACK` end-address computation

Delete the `unsafe {}` (keep the explanatory comment, reworded from SAFETY to
why-the-value-is-right). Verify each with a build (below).

## Step 2 — akuma-cpu gains named fns; kernels call them (≈13–15 sites)

Add to crates/akuma-cpu/src/lib.rs (it is ONE flat lib.rs with both archs
already; follow the existing style: closed, named, doc-commented):

- `ctlreg` writers (the crate currently reads CR0..CR4 only; writes were
  deliberately unsafe at call sites — replace call sites with these NAMED
  writers, one semantic per fn, doc comment states the exact bits):
  - `ctlreg::enable_fpu()` — CR0.EM=0, CR0.MP=1, CR0.NE-ish bits used by
    amd64/src/smp.rs:944 and tk lib.rs:617 (`enable_cpu_features`)
  - `ctlreg::enable_xsave()` — CR4.OSXSAVE (1<<18), amd64/src/smp.rs:994
  - `ctlreg::enable_smep_smap()` — CR4.SMEP|SMAP per CPUID, amd64/src/uaccess.rs:194
  - `ctlreg::set_cr0_wp()` — CR0.WP (1<<16), tk lib.rs:1834 and amd64/src/uaccess.rs:212
- `interrupts::{on, off}` — `sti` / `cli` (nomem, nostack, preserves_flags):
  amd64/src/lapic.rs:406,671,682; amd64/src/boot.rs:339; amd64/src/clock.rs:455
- `park::halt()` — `cli; hlt` loop: amd64/src/main.rs:490, tk lib.rs:1998
  (check akuma-cpu already has a park module with wfi/wfi_mwait — extend it)
- `rdrand() -> Option<u64>` — CPUID-gated: amd64/src/net.rs and
  crates/akuma-tokio-kernel-amd64/src/net.rs have byte-identical asm
- `msr::set_kernel_gs_base(base)` — canonical-checked like the existing
  set_fs_base/set_user_gs_base: amd64/src/smp.rs:204
- PAT init named fn for amd64/src/multiboot2.rs:299 `Msr::Pat` write
  (if a PAT helper exists elsewhere, reuse it instead)

Then replace each call site. Kernel selftest blocks in amd64/src/sched.rs that
use sti/cli may also move — only if it does not change test semantics.

Do NOT do audit steps 3–7 (PhysSlice, SVM, AP bring-up, ring3, gdt per-cpu,
lapic, pte walker) — later stages.

## Verification (KVM is available on this box — use it)

- `cargo build` style checks the repo uses; find the exact amd64 build command
  from CLAUDE.md / amd64/run.sh comments and use it for BOTH targets affected:
  the amd64 kernel and akuma-tokio-kernel-amd64. Also build the aarch64 target
  if cheap, since akuma-cpu is shared.
- Boot-verify on KVM: the repo's amd64 run path supports KVM acceleration on
  this machine (AMD SVM). Run the standard amd64 boot/selftest suite and the
  akuma (tokio-kernel) boot; confirm selftests pass (`boot suite` / `Verdict`
  per the repo's docs). Capture the last ~30 lines of each boot log into the
  final report.
- Gate: `python3 scripts/check_unsafe_budget.py` must pass; then
  `python3 scripts/check_unsafe_budget.py --update` and INCLUDE the budget
  diff in a commit — the ratchet must show the decrease (expected: amd64/src
  80→≈67, akuma-tokio-kernel-amd64 29→≈22, and small named-fn increases
  inside akuma-cpu only).
- Also run `python3 scripts/cloc_akuma.py --json --by-file amd64/src crates/akuma-tokio-kernel-amd64 crates/akuma-cpu` and record site counts before/after.

## Finish

1. `git push litter unsafe-kernel-reduction` (the `litter` remote already points at git@github.com:netoneko/akuma-litter.git).
2. Write docs/archive/UNSAFE_KERNEL_CPU_STEP.md summarizing: per-site table
   (old site → new fn), build+KVM boot evidence, gate before/after counts,
   anything you chose NOT to move and why.
3. End your final reply with: BRANCH=unsafe-kernel-reduction PUSHED=yes|no,
   SITES before/after numbers, and the one-line boot verdict.

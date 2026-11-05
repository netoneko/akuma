# Unsafe audit: the kernels and kernel crates — what to isolate, what to reduce, what stays

**Date:** 2026-11-05. **Scope:** the kernel-crate family (`akuma-kernel-core`,
`akuma-kernel-glue`, `akuma-tokio-kernel{,-core,-amd64,-aarch64}`, `akuma-kacho`,
`akuma-entry`) and the kernel source trees (`src/`, `amd64/src`,
`taoxichuan/src`, `shanghai/src`); production sites only (the
`scripts/check_unsafe_budget.py` lexer's definition — feature-gated kernel
selftests count, `#[cfg(test)]` does not).
**Status:** survey complete; **no code moved; candidates only**. Follows
[`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md), [`POST_AMD64_CODE_AUDIT.md`](POST_AMD64_CODE_AUDIT.md)
§2.3 and [`TAOXICHUAN_UNSAFE_REDUCTION_PLAN_GLM.md`](TAOXICHUAN_UNSAFE_REDUCTION_PLAN_GLM.md)
as the next round of the same question.

## Why

The AArch64 half of the tree finished its unsafe extraction: `src/` and
`taoxichuan/src` both measure **0 production sites**, and every kernel-family
crate except two carries `#![forbid(unsafe_code)]`. The unsafe surface left in
the *kernels and kernel crates* is now entirely the amd64 port:

| component | production sites | forbids |
|---|---:|---|
| `amd64/src` | 80 | no |
| `crates/akuma-tokio-kernel-amd64` | 29 | no |
| `crates/akuma-kernel-core` | 0 | yes |
| `crates/akuma-kernel-glue` | 0 | yes |
| `crates/akuma-tokio-kernel` | 0 | yes |
| `crates/akuma-tokio-kernel-core` | 0 | yes |
| `crates/akuma-tokio-kernel-aarch64` | 0 | yes |
| `crates/akuma-kacho` | 0 | yes |
| `src/` (AArch64 kernel) | 0 | — |
| `taoxichuan/src` | 0 | — |
| `shanghai/src` | 0 (1 mention, a doc comment) | — |
| `crates/akuma-entry` | 0 sites (extern decls + `global_asm!` only) | no, by design |

**Total in scope: 109 sites, all in two files' worth of crates.** This audit
reads every one of them and sorts them into three buckets: **isolatable**
(the contract belongs in a crate that already exists — move it), **reducible**
(the `unsafe` block is unneeded, duplicated, or replaceable by an existing safe
API), and **must-stay** (terminal or the designed isolation edge).

## Method

Three passes. (1) A `rg "unsafe \{|unsafe fn|unsafe extern|unsafe impl"` census
over the scope, cross-checked against `scripts/cloc_akuma.py --json --by-file`
so test-only hits are excluded the same way the budget gate excludes them.
(2) A context read of every production site (±8 lines) to classify it.
(3) A check of what the owning crates already export (`akuma-cpu`,
`akuma-pagetable`, `akuma-lapic-amd64`, `akuma-ring3-amd64`, `akuma-gdt`,
`akuma-boot-amd64::phys`, `akuma-user-access`, `akuma-primitives::mmio`,
`sched::with_current_uctx`) so every "move" below lands in a crate that exists,
not a new one — per the policy in `CLAUDE.md`: *isolate `unsafe` in small crates
whose only job is one stated contract*.

## Per-file inventory (production sites)

| file | sites | what they are |
|---|---:|---|
| `amd64/src/smp.rs` | 10 | GsBase write, 3 `extern "C"` symbol groups, AP-root/trampoline copies, mailbox write, CR0/CR4 asm, `activate` call |
| `amd64/src/kvm.rs` | 9 | SVM enable pair, 5 `vmrun` call sites, `vmrun` fn + asm |
| `amd64/src/uaccess.rs` | 6 | CR0/CR4 writes, 2 shared-copy calls, `read_val`'s `MaybeUninit` pair |
| `amd64/src/sched.rs` | 6 | `activate` decl + 2 calls, selftest counter/sti/cli blocks |
| `amd64/src/multiboot2.rs` | 6 | 2 `MmioWindow::new`, boot.s PD extern, `Table::at`, `Msr::Pat` write, image-bytes slice |
| `amd64/src/paging.rs` | 5 | `activate`/`activate_unpublished` decls, `drop_identity` call, 2 smoke-test phys reads |
| `amd64/src/lapic.rs` | 3 | 2× `ApicBase \| GLOBAL_ENABLE`, `sti` |
| `amd64/src/gdt.rs` | 3 | per-CPU TSS/GDT raw-pointer pokes (1 is pure address math) |
| `amd64/src/usermode.rs` | 4 | leave-asm extern, `.Lexit_to_kernel` unwind asm, SCE/STAR/LSTAR MSR block, dispatch call |
| `amd64/src/kvm/linux.rs` + `api/` + `machine.rs` | 6 | `vmrun_regs` fn + asm, `vmsave`, 2 call sites, `Box<[T;1]>` layout trick |
| `amd64/src/idt.rs` | 2 | crash-dump frame read, physmap readback |
| `amd64/src/{shootdown,signal,shmpages,ramdisk,net,kbd,loader}.rs` | 2 each | CR3 self-flush; extern; 2 `uctx` derefs; 2 phys copies; 2 physmap slices; Waker vtable + `rdrand`; bounded-lock `Sync` + `get`; 2 user-page copies |
| `amd64/src/{boot,clock,main,machine,reboot,blk,power}.rs` | 1 each | `sti` ×2, `cli;hlt`, physmap read, `lidt`+`int3` reset, device-window read, EC-window read |
| `akuma-tokio-kernel-amd64/src/lib.rs` | 14 | physmap read/r/w, `trap_top_for` (address math only!), TSS/GDT init ×2, 4-level walk ×3, `ApicBase`, `MmioWindow`, CPU-feature CR0 ×2, `halt` |
| `akuma-tokio-kernel-amd64/src/smp.rs` | 7 | 2 extern groups, CR3 self-flush, mailbox, AP-root copy, trampoline copy, `ApicBase` |
| `akuma-tokio-kernel-amd64/src/lazy.rs` | 3 | 3× `Frame::from_pa` walker glue |
| `akuma-tokio-kernel-amd64/src/{sched,exec_twin,net}.rs` | 2/2/1 | `activate` decl + open-coded `mov cr3`; `uctx` deref + `enter_user` call; `rdrand` (a byte-for-byte duplicate of `amd64/src/net.rs`'s) |

## Findings

### A. Zero-cost removals — the `unsafe` is not needed at all (2 sites)

Both blocks contain only address arithmetic, which is safe:

1. `akuma-tokio-kernel-amd64/src/lib.rs:170` `trap_top_for` — `&raw const` +
   `cast` + `add` + `as u64`. The SAFETY comment even says "nothing is read
   through it". Drop the `unsafe`.
2. `amd64/src/gdt.rs:100` — `(&raw mut TRAP_STACK).cast::<u8>().add(N) as u64`.
   Same shape; the comment documents why the *value* is right, not an unsafety
   obligation. Drop the `unsafe`.

Mechanical, no design, −2 sites, and it stops the lexer counting them forever.

### B. Isolatable — the contract belongs in a crate that already exists

The house policy is that `unsafe` lives in a leaf crate with one stated
contract (`akuma-cpu`, `akuma-msr`, `akuma-dma`, `akuma-trap-amd64` are the
precedents). Every item below moves sites *out* of the kernels into an existing
crate; the kernel keeps safe calls. Approximate yields:

| lever | move | sites it empties | crate that takes them |
|---|---|---:|---|
| **B1. SVM run loop** | `vmrun`/`vmrun_regs`/`vmsave` + the `EFER.SVME`/`VM_HSAVE_PA` enable pair, plus their 7 call sites | 14 | `akuma-svm-amd64` (already owns the VMCB bit vocabulary; `kvm/linux.rs` already has the `vmrun_regs` shape — promote it) |
| **B2. physmap slices** | `phys_ptr` + volatile/copy at a proven-RAM PA: machine.rs, loader.rs ×2, idt.rs, shmpages.rs ×2, ramdisk.rs ×2, multiboot2.rs, paging smoke ×2, tk lib.rs ×4 | ≈14 | `akuma-boot-amd64::phys` gains a bounds-checked `PhysSlice { pa, len }` with safe `read`/`write`/`copy_*` — one `unsafe` inside the phys crate, exactly the `DmaArray`/`MmioWindow` pattern |
| **B3. AP bring-up** | trampoline symbols (`ap_trampoline_*`, `__ap_pml4`, `__pdpt_low`), `install_trampoline`, `mailbox_write`, `ap_boot_root` copies — duplicated wholesale between `amd64/src/smp.rs` and `akuma-tokio-kernel-amd64/src/smp.rs` today | ≈11 | `akuma-boot-amd64` (it owns `boot.s`, so the extern symbols' contract is already itsn) as an `ap` module; both kernels call it |
| **B4. CR0/CR4 feature bits** | SMEP/SMAP enable, XSAVE enable, CR0.EM/MP/NE, CR0.WP ×2, `enable_cpu_features` | 6 | `akuma_cpu::ctlreg` — the crate already owns CR reads "read only; writes stay unsafe at the call site"; replace with *named* writers (`enable_smap_smep`, `enable_xsave`, `set_cr0_wp`, `fpu_bits`), the same asymmetry `akuma_cpu::msr` already draws |
| **B5. `sti`/`cli`/halt** | 6 sites in `amd64/src` (lapic ×3, boot, clock, main) + `halt` in tk lib.rs (+ optionally 2 in sched selftests) | 7–9 | `akuma-cpu` — the AArch64 side already has `daif` and `park::{wfi_mwait}`; an `interrupts::{on, off}` + `park::halt` closes the same gap on amd64 |
| **B6. Ring-crossing plumbing** | usermode's leave-extern, `.Lexit_to_kernel` unwind asm, SCE/STAR/LSTAR MSR block; `shootdown_entry`/`wake_ipi_entry` externs | 6 | `akuma-ring3-amd64` (owns `enter_user`; the unwind asm is its `.Lexit_to_kernel` by construction) and `akuma-trap-amd64`/`akuma-idt-amd64` (the registered-entry pattern that already took 18 `&mut *frame` sites) |
| **B7. Per-CPU TSS/GDT** | `amd64/src/gdt.rs` ×3 (one is §A-free), tk lib.rs ×2 (`set_rsp0` + the GDT/TSS init block) | 5 | `akuma-gdt` gains `PerCpuGdt` owning the statics and the "only this core touches its slot, IF masked" contract once |
| **B8. `ApicBase \| GLOBAL_ENABLE`** | the same bit-or-const write appears 4× (lapic ×2, tk ×2) | 4 | `akuma-lapic-amd64::global_enable()` — it already exports the constant; let it own the write |
| **B9. boot.s page directories** | `multiboot2.rs` extern + raw `Table::at` construction | 2 | `akuma-boot-amd64` (owns `boot.s`); expose the typed accessor, keep `MmioWindow` sites as they are |
| **B10. SVM enable** is inside B1 | — | — | — |

Sum of B: **≈52 of the 80 `amd64/src` sites and 14 of the tk sites** move into
crates that already exist and already hold adjacent contracts. The new unsafe
added to those crates is small and named (one `PhysSlice` impl, one `ap`
module, a handful of `#[inline(always)]` instruction wrappers) — the
`check_unsafe_budget.py` budget diff will show exactly that transfer into
review.

### C. Reducible — delete or call an existing safe API (no contract changes)

| fix | sites | how |
|---|---:|---|
| tk `sched.rs:84` open-codes `mov cr3` next to `akuma_pagetable::amd64::load_cr3` | 1 | call `load_cr3`, or add a safe `pagetable::flush_non_global()` (CR3 self-rewrite is architectural, not caller-specific) — `shootdown.rs:141` takes the same fn (−1 more) |
| `signal.rs` ×2 and `exec_twin.rs` ×2 dereference `current_uctx()` raw | 2 of 4 | `sched::with_current_uctx` exists *specifically* to replace these blocks (`sched.rs:1700` documents the nine it already ate) — `exec_twin`'s `enter_user` call stays (it is the crate edge) |
| `uaccess::read_val`'s `from_raw_parts_mut` + `assume_init` pair | 2 | move `read_val`/`write_val` into `akuma-user-access` (it owns `copy_from_user_safe` and the fault-fixup contract; the MaybeUninit dance belongs beside it) |
| `rdrand` duplicated byte-for-byte in `amd64/src/net.rs` and tk `net.rs` | 2 | one `akuma_cpu::rdrand() -> Option<u64>`; both callers keep the CPUID gate |
| `GsBase` raw write (`smp.rs:204`) | 1 | `set_kernel_gs_base(base)` behind the same canonical check `set_fs_base`/`set_user_gs_base` already do in `akuma_cpu::msr` |
| `Msr::Pat` write (`multiboot2.rs:299`) | 1 | a named `akuma_cpu` PAT-init fn next to `ctlreg` |
| `blk.rs`/`power.rs` volatile reads through addresses `init` already mapped | 2 | keep the `MmioWindow`/window handle and use its checked accessors instead of re-deriving the pointer |
| `CR3` self-flush in tk `smp.rs:164` | 1 | same safe `flush_non_global()` as above |
| 4-level walk duplicated in tk `lib.rs` (`leaf_entry`, the CR3 walk) vs `akuma_pte_amd64`'s | 2 | reuse the pte crate's walker; `lazy.rs`'s three `Frame::from_pa` stay (they are the adapter edge, below) |

Sum of C: **≈12 sites**, plus the 2 free ones in §A.

### D. Must-stay (with the SAFETY comments that make them auditable)

These are terminal operations, the designed isolation edge, or one-shot
contracts the tree has already deliberately placed:

- **`reboot.rs:95`** — `lidt` an empty IDT + `int3` to reset. Terminal by
  intent; the comment is the spec.
- **`paging.rs` `activate`/`activate_unpublished` decls and their 3 call
  sites; sched's `activate` trait decl and calls; `activate` call in
  tk `sched.rs`** — this *is* the address-space isolation edge; the caller's
  obligation ("a live PML4 that maps the kernel half") is stated once.
- **`MmioWindow::new` sites (multiboot2 ×2, tk lib.rs:585)** — the design is
  one `unsafe` vouch per mapped window (CLAUDE.md, `akuma-primitives::mmio`).
  Keep.
- **`akuma-user-access` call sites (`uaccess.rs:297/311`)** — calls into the
  shared copy loop's `unsafe fn`; the contract lives in the crate.
- **`exec_twin.rs:451` `enter_user`** — the ring-3 crossing's own edge.
- **`net.rs` no-op `Waker` vtable** — the standard construction; `from_raw`
  here documents the std contract, not a kernel one.
- **`kbd.rs` `BoundedLock`** (`unsafe impl Sync` + `state.get`) — a deliberate
  bounded spinlock with a spin budget so an I8042 wedge degrades instead of
  hanging; the budget is the feature. Note only: if a second driver grows this
  shape, promote it to a shared type rather than copying the `unsafe impl`.
- **`kvm/api.rs:409`** `Box<[T; 1]> → Box<T>` layout trick — documented std
  layout equality.
- **`idt.rs:151` `stack_word`** — crash-dump frame reads; converting the dump
  path to a `#[repr(C)]` frame view is possible but is debugging-path churn
  with crash-day risk. Keep.
- **`lazy.rs` ×3 `Frame::from_pa`** — the loader-adapter's vouch that a walked
  entry names a live frame; this is the isolation edge doing its job.

**After B + C + A: `amd64/src` ≈ 80 → ~22, tk ≈ 29 → ~6. Total ≈ 109 → ~28
(−74%)**, all of the remainder in bucket D except the selftest blocks, which
shrink further if B5's `interrupts::{on,off}` is adopted there too.

## Ranked plan

1. **§A first** — 2 sites deleted, zero risk, today.
2. **B4/B5 (akuma-cpu: ctlreg writers + interrupts/halt)** — 13–15 sites,
   mechanical, and it kills the last "open-coded instruction outside the
   instruction crate" cases the CLAUDE.md rules exist to prevent.
3. **B8 + C's `rdrand`/`GsBase`/`Pat`** — 7 sites, one-line fns.
4. **B2 (PhysSlice)** — the biggest single structural move (≈14 sites); do it
   with the `DmaArray` precedent in hand and a boot-suite run; it also
   simplifies `ramdisk`'s `&self → &mut` aliasing note.
5. **B1 (SVM into `akuma-svm-amd64`)** — 14 sites; `kvm/linux.rs::vmrun_regs`
   is already the right shape, promote rather than rewrite; then `kvm.rs`'s
   private `vmrun` can be deleted outright.
6. **B3 (AP bring-up into `akuma-boot-amd64::ap`)** — 11 sites *and* it
   de-duplicates two kernels' SMP paths that currently must be kept in lockstep
   by hand; review each diff for the CR3-below-4-GiB and identity-slot rules.
7. **B6/B7/B9 + remaining C** — the tail.

Each step ends with `python3 scripts/check_unsafe_budget.py --update` so the
budget file's diff carries the decrease (the ratchet never lets it creep back),
and a boot-suite/forktest run on both targets per the repo convention.

## Not covered here

- Crates outside the kernel family (`akuma-alloc` 24, `akuma-cpu` 65,
  `akuma-exceptions` 29, …) — in the budget file, audited by
  [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md) and the crate-safety reference.
- The *host* KVM backend's Linux ioctl path (`amd64/src/kvm/linux.rs` beyond
  the SVM asm): its ioctl marshalling sites are B1-adjacent but belong to the
  next pass with `akuma-kvm-abi`.
- `akuma-entry` keeps its extern/`global_asm!` on purpose (0 blocks; the
  crate header is the contract).

## Background

- [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md) — the original tree-wide census and the
  isolate/reduce vocabulary this doc reuses.
- [`POST_AMD64_CODE_AUDIT.md`](POST_AMD64_CODE_AUDIT.md) §2.3 — asked for the
  budget gate; `docs/reference/unsafe-budget.json` + `scripts/check_unsafe_budget.py`.
- [`AMD64_UNSAFE_REDUCTION_TRAP_MSR.md`](AMD64_UNSAFE_REDUCTION_TRAP_MSR.md),
  [`SRC_BOOT_ENTRY_UNSAFE_CLEANUP.md`](SRC_BOOT_ENTRY_UNSAFE_CLEANUP.md),
  [`SYSCALL_UNSAFE_CLEANUP.md`](SYSCALL_UNSAFE_CLEANUP.md) — the per-crate
  precedents for exactly the moves in §B.
- [`INLINE_ASM_CLEANUP.md`](INLINE_ASM_CLEANUP.md) — the 218 → 35 `asm!` fold
  that `akuma-cpu`'s amd64 additions would finish.
- `docs/reference/crate-safety.md` — which 79 crates forbid and why.

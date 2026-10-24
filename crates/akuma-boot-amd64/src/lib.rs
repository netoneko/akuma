//! The amd64 boot kit: PVH entry, the physmap, kernel page tables, heap + PMM.
//!
//! Extracted 2026-10-24 from `amd64/src/{boot.s,phys.rs,paging.rs,mem.rs}` as
//! stage 1 of the minimal-self-hosting experiment
//! (`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`): a composed kernel
//! (**Taoxichuan**, `taoxichuan/`) pulls these as crates rather than inheriting
//! the `akuma-amd64` module tree, and `akuma-amd64` itself now pulls the same
//! crate — one boot block, two kernels, no drift.
//!
//! What is here, and why it had to be a crate rather than a module:
//!
//! * **`boot.s`** — the PVH/multiboot2 trampoline: the Xen ELF note that makes
//!   a VMM enter us 32-bit, the boot page tables, the long-mode far jump, the
//!   high entry that hands `kmain` the start-info pointer. Instantiated here
//!   with `global_asm!` (the only caller of the `{PDS}` template), so any bin
//!   that links this crate gets `_start` and the boot `.bss` for free.
//! * **`phys`** — the three address windows (physmap, device, kernel image) and
//!   `phys_to_virt`, plus `FreshFrame`.
//! * **`paging`** — the kernel's *own* mappings: map/unmap/translate in the
//!   active root, the device-window mapper. Not user address spaces (those are
//!   `akuma-mmu`'s) and not `activate` (that publishes to `akuma-mmu`'s live-L0
//!   registry and belongs to whichever kernel has one).
//! * **`mem`** — heap-then-PMM bring-up over a `MachineDescription`: every
//!   region managed across the MMIO gap, the heap placed beside the kernel,
//!   the boot-log accounting of what became of each region. Diagnostics go
//!   through `akuma_primitives`' print hook — the caller's console, whatever it
//!   is, not this crate's opinion of one.
//!
//! The one contract the caller still owns: an `extern "C" kmain(u64)` (and
//! `kmain_mb2(u64)` for the multiboot2 path) must exist in the linking binary —
//! `boot.s` calls them by symbol.

#![cfg_attr(not(test), no_std)]
#![allow(unsafe_code)]
// Same two allows as the sibling hardware crates (akuma-bkl, akuma-elf, akuma-exec): the module docs here
// are long on purpose, and the page-table helpers return values their callers always use.
#![allow(clippy::too_long_first_doc_paragraph, clippy::must_use_candidate)]

pub mod mem;
pub mod paging;
pub mod phys;

// `PDS` is the one number `boot.s` is not allowed to know on its own: it sizes
// the boot page directories, and `phys::PHYSMAP_PDS` is what every
// `phys_to_virt` is checked against. Passing it in as a `const` operand makes
// them the same value rather than two values that agree today. `boot.s`
// deliberately contains no other brace, so the whole file is a valid format
// template.
// x86_64 only: the trampoline is AT&T x86 assembly, and the host-side clippy/test pass in the
// pre-commit hook runs this crate on whatever the developer's machine is (an aarch64 Mac).
#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    include_str!("boot.s"),
    PDS = const phys::PHYSMAP_PDS,
    options(att_syntax)
);

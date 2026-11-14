//! The per-core GDT and TSS, and the two instructions that load them.
//!
//! `akuma-gdt` is the descriptors as data (pure, `forbid(unsafe_code)`); this crate is the `static`
//! storage and `lgdt`/`ltr`. It exists so a kernel crate can forbid `unsafe` and still have a TSS:
//! `akuma-tokio-kernel-amd64` holds none of this any more (`docs/archive/UNSAFE_FORBID_THREE_CRATES_PLAN.md`).
//!
//! # The one contract, and why the API cannot break it
//!
//! > Core `n` is the only code that ever touches slot `n` of the two tables, and it does so with
//! > interrupts masked.
//!
//! * [`init_cpu`] takes an explicit index (the BSP runs it before `gs:` exists) and **claims the slot
//!   once**: a second call for the same index panics, so no two cores can both build one row. Naming
//!   another core's index is wrong, but it can only make *that core's own* later init panic, never race a
//!   running one.
//! * [`set_rsp0`] takes no index at all: it asks `gs:` which core it is on, so it can only write the
//!   caller's own TSS. Its callers have `IF` clear (a context switch).

#![no_std]
// The statics and the two instructions below are the whole point of the crate.
#![allow(unsafe_code)]

#[cfg(target_arch = "x86_64")]
use akuma_gdt::{GDT_LIMIT, SLOTS, TSS_SELECTOR, Tss};
#[cfg(target_arch = "x86_64")]
use core::sync::atomic::{AtomicBool, Ordering};

/// Most cores the tables hold. Same bound as `akuma_ring3_amd64::MAX_CPUS`.
pub const MAX_CPUS: usize = 32;

#[cfg(target_arch = "x86_64")]
static mut TSS: [Tss; MAX_CPUS] = [const { Tss::new() }; MAX_CPUS];
#[cfg(target_arch = "x86_64")]
static mut GDT: [[u64; SLOTS]; MAX_CPUS] = [[0; SLOTS]; MAX_CPUS];
#[cfg(target_arch = "x86_64")]
static CLAIMED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

#[cfg(target_arch = "x86_64")]
#[repr(C, packed)]
struct Gdtr {
    limit: u16,
    base: u64,
}

/// Build core `cpu`'s GDT and TSS with `rsp0` as its initial ring-3 trap stack, and load both on the
/// calling core. The caller runs on core `cpu` with interrupts masked.
///
/// No far return to reload `CS`: the table's code and data entries are byte-identical to the ones the boot
/// code installed, so the selectors already loaded stay valid.
///
/// # Panics
/// If `cpu >= MAX_CPUS`, or `cpu`'s slot was already initialised.
#[cfg(target_arch = "x86_64")]
pub fn init_cpu(cpu: usize, rsp0: u64) {
    assert!(cpu < MAX_CPUS, "cpu index out of range for the GDT table");
    assert!(!CLAIMED[cpu].swap(true, Ordering::AcqRel), "GDT slot initialised twice");
    // SAFETY: the slot was claimed just above, so no other `init_cpu` touches it, and `set_rsp0` only
    // ever writes the calling core's own slot after this core has initialised it. The statics are reached
    // through raw pointers, never references to the whole array.
    unsafe {
        let tss = (&raw mut TSS).cast::<Tss>().add(cpu);
        (*tss).set_rsp0(rsp0);
        let gdt = (&raw mut GDT).cast::<[u64; SLOTS]>().add(cpu);
        *gdt = akuma_gdt::table(tss as u64);
        let gdtr = Gdtr { limit: GDT_LIMIT, base: gdt as u64 };
        core::arch::asm!(
            "lgdt [{gdtr}]",
            "ltr {tr:x}",
            gdtr = in(reg) &raw const gdtr,
            tr = in(reg) TSS_SELECTOR,
            options(readonly, nostack, preserves_flags)
        );
    }
}

/// Point THIS core's TSS `rsp0` at `top`: where a ring-3 trap on this core lands. The core is the caller's,
/// found through `gs:`, so no other core's slot can be named.
///
/// # Panics
/// If this core never ran [`init_cpu`].
#[cfg(target_arch = "x86_64")]
pub fn set_rsp0(top: u64) {
    let cpu = akuma_cpu::percpu::core_id() as usize;
    assert!(cpu < MAX_CPUS && CLAIMED[cpu].load(Ordering::Acquire), "set_rsp0 before this core's init_cpu");
    // SAFETY: `cpu` is the calling core, whose slot `init_cpu` built and nothing else touches; the CPU reads
    // `rsp0` on this core's next ring-3 trap, not concurrently, and callers run with `IF` clear.
    unsafe {
        (*(&raw mut TSS).cast::<Tss>().add(cpu)).set_rsp0(top);
    }
}

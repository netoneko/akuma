//! GDT and TSS: the descriptors ring 3 needs — one set per core.
//!
//! `boot.s` builds a three-entry GDT — null, kernel code, kernel data — which is
//! everything long mode requires and nothing userspace does. This replaces it
//! with the full table.
//!
//! # Layout, and why the order is not free (`akuma-gdt` has the data and the proof)
//!
//! ```text
//! 0x00  null
//! 0x08  kernel code   (64-bit, DPL 0)
//! 0x10  kernel data   (DPL 0)
//! 0x18  user data     (DPL 3)
//! 0x20  user code     (64-bit, DPL 3)
//! 0x28  TSS           (system descriptor, 16 bytes — occupies 0x28 and 0x30)
//! ```
//!
//! `sysret` does not take selectors; it *computes* them from `IA32_STAR[63:48]`,
//! loading `CS = base + 16` and `SS = base + 8`. With base `0x13` (`0x10 | 3`)
//! that yields `CS = 0x23` and `SS = 0x1b`, which is why user **data** must sit
//! immediately below user **code** rather than the intuitive other way round.
//! Swap those two rows and `sysret` lands in ring 3 with a data selector in
//! `CS`. See [`SYSRET_BASE`] for why the RPL belongs in the base.
//!
//! `syscall` is the mirror image: it loads `CS = IA32_STAR[47:32]` and
//! `SS = that + 8`, so kernel code at `0x08` and kernel data at `0x10` are also
//! forced adjacent. Both constraints are on *this* table. Nothing checks them when
//! the table is loaded, so `akuma-gdt` states them as compile-time assertions and
//! host tests instead of leaving a wrong order to fault on the first transition.
//!
//! # Why the kernel entries are byte-identical to `boot.s`
//!
//! Deliberate: it means `CS` stays valid across the `lgdt`, so there is no need
//! to reload it with a far return. The only reload is `ltr`. The AP trampoline
//! (`boot.s`, `ap_gdt`) keeps the same two entries at the same selectors for
//! the same reason: a secondary arrives in long mode with `CS = 0x08`, loads its
//! table here, and nothing about its segment state changes.
//!
//! # One table per core
//!
//! The TSS is what forces this. `rsp0` — the stack the CPU switches to on a
//! ring-3 trap — is per *task*, rewritten on every context switch by
//! [`set_kernel_stack`]; with two cores running two tasks, one TSS would have
//! each switch overwrite the other core's trap stack. `ltr` on a descriptor
//! marks it busy, too, so two cores cannot even load the same one. The four
//! segment descriptors are identical across cores and could be shared; a table
//! per core is simpler than one table with a TSS slot per core, and costs
//! `7 * 8 * MAX_CPUS` bytes.

use crate::smp::MAX_CPUS;

pub use akuma_gdt::{USER_CS, USER_SS};

/// Stack the BSP switches to for a ring-3 → ring-0 trap before the scheduler
/// has installed a task's own. Separate from the boot stack because it must be
/// valid whatever userspace did to its own. Secondaries use their idle task's
/// stack for the same purpose.
const KERNEL_TRAP_STACK_SIZE: usize = 16 * 1024;

/// The field is never *read* — the stack is addressed through a raw pointer to
/// the whole struct — but it must exist to reserve the bytes, and `repr(align)`
/// is what the CPU requires of a stack it switches to.
#[repr(align(16))]
struct TrapStack(#[allow(dead_code)] [u8; KERNEL_TRAP_STACK_SIZE]);

static mut TRAP_STACK: TrapStack = TrapStack([0; KERNEL_TRAP_STACK_SIZE]);

const _: () = assert!(MAX_CPUS <= akuma_percpu_gdt_amd64::MAX_CPUS, "the GDT/TSS table holds fewer cores than smp");

/// Point this core's TSS at the stack a ring-3 trap should switch to.
///
/// Must be called on every context switch once preemption exists. `rsp0` is
/// where the CPU pushes the interrupt frame when an interrupt arrives **in ring
/// 3**, and a preempted task is suspended *on that frame* — so two tasks sharing
/// one `rsp0` would have the second's interrupt overwrite the first's saved
/// state, and the first would resume into a frame describing the second.
///
/// The core is found through `gs:` by `akuma-percpu-gdt-amd64`, so only this
/// core's own slot can be written.
pub fn set_kernel_stack(rsp0: u64) {
    akuma_percpu_gdt_amd64::set_rsp0(rsp0);
}

/// Build the BSP's GDT and TSS, load both.
pub fn init() {
    // The static trap stack's end, as an integer for the TSS — pure address
    // arithmetic on a `static` (a raw pointer, a cast, an offset; no
    // dereference), so no `unsafe` block. `wrapping_add` is the safe spelling
    // of the sum; a static's own end is by definition the last byte of it.
    let rsp0 = (&raw mut TRAP_STACK)
        .cast::<u8>()
        .wrapping_add(KERNEL_TRAP_STACK_SIZE) as u64;
    init_cpu(0, rsp0);
}

/// Build core `idx`'s GDT and TSS, with `rsp0` as its initial trap stack, and
/// load both on the calling core (`akuma_percpu_gdt_amd64::init_cpu`: each slot
/// is claimed once, so a second init of a core panics rather than racing it).
///
/// The BSP calls this as core 0 before `smp::init_bsp`, so it must not read
/// `gs:` — it takes the index as an argument for that reason.
pub fn init_cpu(idx: usize, rsp0: u64) {
    assert!(idx < MAX_CPUS, "cpu index out of range for the GDT table");
    akuma_percpu_gdt_amd64::init_cpu(idx, rsp0);
}

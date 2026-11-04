//! The kernel's part of a task's address space, and switching `TTBR0_EL1` to one.
//!
//! The kernel runs identity-mapped through TTBR0 (`_boot`'s tables), so a task's
//! root carries the kernel too, in two places:
//!
//! * root slot [`DEVICE_SLOT`] (`L0[1]`): the device window, the boot table's own
//!   L1, shared by every root;
//! * the RAM identity, entries [`RAM_SLOTS`] of the task's own `L0[0]` table:
//!   the boot table's 1 GiB blocks, EL1-only, made user-no-execute here.
//!
//! The task owns everything else: `L0[0]`'s other entries (its image and
//! interpreter below 1 GiB), and every other root slot (its `mmap` window and
//! stack). [`kernel_entry`] names the kernel's entries inside the task's table so
//! the fork clone copies them verbatim and the teardown never frees them.
//!
//! [`switch_to`] is the checked `msr ttbr0_el1`: it refuses a root without the
//! kernel's part, so a wrong root is a `false`, not a core executing from a
//! table that does not map it.

use core::ops::Range;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

use akuma_phys::Physmap;

/// The valid bit and the table bit of a descriptor.
const VALID: u64 = 1;
const TABLE: u64 = 1 << 1;
/// Unprivileged execute-never.
const UXN: u64 = 1 << 54;
/// A descriptor's output address.
const ADDR: u64 = 0x0000_ffff_ffff_f000;

/// The root slot of the device window.
pub const DEVICE_SLOT: usize = 1;
/// The entries of `L0[0]`'s table that hold the kernel's RAM identity: 1 GiB
/// blocks over `[1 GiB, 3 GiB)`.
pub const RAM_SLOTS: Range<usize> = 1..3;

static BOOT_ROOT: AtomicU64 = AtomicU64::new(0);

/// Record the boot root (the live `TTBR0_EL1`). Once, before the first switch.
pub fn init() {
    if BOOT_ROOT.load(Relaxed) == 0 {
        BOOT_ROOT.store(akuma_cpu::sysreg::ttbr0_el1() & ADDR, Relaxed);
    }
}

/// The boot root: the kernel's own address space, with no task in it.
#[must_use]
pub fn boot_root() -> u64 {
    BOOT_ROOT.load(Relaxed)
}

fn entry(table: u64, i: usize) -> u64 {
    akuma_pagetable::entry::<Physmap>(table, i)
}

/// The kernel's RAM-identity entry `i` of `L0[0]`, as a task's table holds it.
fn ram_block(i: usize) -> u64 {
    let boot_l1 = entry(boot_root(), 0) & ADDR;
    let e = entry(boot_l1, i);
    if e & VALID == 0 { 0 } else { e | UXN }
}

/// Is the entry at `depth` (3 = a root entry ... 0 = a 4 KiB leaf) that covers
/// `va` the kernel's, inside a task's own table?
#[must_use]
pub const fn kernel_entry(depth: u32, va: u64) -> bool {
    depth == 2 && va >= (RAM_SLOTS.start as u64) << 30 && va < (RAM_SLOTS.end as u64) << 30
}

/// Is root slot `i` the task's?
#[must_use]
pub const fn user_root_slot(i: usize) -> bool {
    i != DEVICE_SLOT
}

/// Give the root `dst` the kernel's part.
///
/// When `L0[0]` is empty (a fresh root) its table comes from `alloc_table` (a zeroed frame). When it is not (a clone
/// copied the parent's), the RAM identity is already in it. `false` when a
/// table frame cannot be had.
pub fn install_kernel(dst: u64, alloc_table: impl FnOnce() -> Option<u64>) -> bool {
    akuma_pagetable::set_entry::<Physmap>(dst, DEVICE_SLOT, entry(boot_root(), DEVICE_SLOT));
    if entry(dst, 0) & VALID == 0 {
        let Some(l1) = alloc_table() else { return false };
        for i in RAM_SLOTS {
            akuma_pagetable::set_entry::<Physmap>(l1, i, ram_block(i));
        }
        akuma_pagetable::set_entry::<Physmap>(dst, 0, (l1 & ADDR) | VALID | TABLE);
    }
    has_kernel(dst)
}

/// Does `root` carry the kernel's part exactly as [`install_kernel`] builds it?
#[must_use]
pub fn has_kernel(root: u64) -> bool {
    if root == boot_root() {
        return true;
    }
    if entry(root, DEVICE_SLOT) != entry(boot_root(), DEVICE_SLOT) {
        return false;
    }
    let l0 = entry(root, 0);
    if l0 & (VALID | TABLE) != VALID | TABLE {
        return false;
    }
    RAM_SLOTS.into_iter().all(|i| entry(l0 & ADDR, i) == ram_block(i))
}

/// Run on `root` (`0` = the boot root): `msr ttbr0_el1`, then drop every
/// non-global translation, since every task runs under ASID 0. `false`, and no
/// switch, when the root lacks the kernel's part.
#[must_use = "false means the switch was refused and the old root is still live"]
pub fn switch_to(root: u64) -> bool {
    let root = if root == 0 { boot_root() } else { root };
    if !has_kernel(root) {
        return false;
    }
    if root == akuma_cpu::sysreg::ttbr0_el1() & ADDR {
        return true;
    }
    // SAFETY: `root` maps the kernel exactly as the boot root does (checked
    // above: the device window and the RAM identity this code runs from), so
    // the instruction after the write fetches through a table that maps it.
    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, {r}",
            "isb",
            "tlbi vmalle1",
            "dsb nsh",
            "isb",
            r = in(reg) root,
            options(nostack, preserves_flags),
        );
    }
    true
}

/// The root this core runs on.
#[must_use]
pub fn live_root() -> u64 {
    akuma_cpu::sysreg::ttbr0_el1() & ADDR
}

/// Name the thread this core runs: `TPIDRRO_EL0`, which
/// `akuma_primitives::preempt::current_tid` reads and indexes every per-thread
/// table with (the user-copy fixup, the preemption counters).
///
/// A tokio kernel's scheduler calls it on every switch-in. EL0 can read the
/// register and cannot write it; it holds a slot index, nothing it could use.
pub fn set_current_thread(tid: usize) {
    // SAFETY: TPIDRRO_EL0 is never dereferenced: every reader bounds-checks the
    // value before indexing (`current_tid` answers it, its callers compare it
    // with `MAX_THREADS`), so any value is at worst a misattribution.
    unsafe {
        core::arch::asm!("msr tpidrro_el0, {t}", t = in(reg) tid as u64, options(nomem, nostack, preserves_flags));
    }
}

//! The aarch64 machine, as the core's memory code sees it: [`Aarch64`]'s
//! answers to `akuma_tokio_kernel_core`'s `UserPages`, `Mm`, `lazy::Kernel` and
//! `tree::UserTree`, over `akuma-pagetable`'s AArch64 walker, `akuma-pte-aarch64`'s
//! leaf encoding and `akuma-boot-aarch64::user`'s kernel part of a root. The
//! amd64 half's `lazy.rs`, for this machine.

use akuma_phys::{FreshFrame, Physmap};
use akuma_tokio_kernel_core::lazy_record::Lazy;
use akuma_tokio_kernel_core::mmu::{FreshPage, UserPages};
use akuma_tokio_kernel_core::task::{Task, Tasks};

use akuma_pagetable::aarch64 as walk;
use akuma_pte_aarch64 as pte;

/// Task records: the spawn ring's slots plus the boot thread's (record 0).
pub use akuma_tokio_kernel_core::proc::NSLOTS;
/// Cores this kernel can run (A6 brings the secondaries).
pub const MAX_CPUS: usize = 8;

static TASKS: Tasks<{ NSLOTS + 1 }, MAX_CPUS> = Tasks::new();

/// This core's index (`MPIDR_EL1.Aff0`).
#[must_use]
pub fn cpu_index() -> usize {
    (akuma_cpu::sysreg::mpidr_el1() & 0xff) as usize % MAX_CPUS
}

/// Which task record the syscall path acts on now.
#[must_use]
pub fn task_slot_index() -> usize {
    TASKS.current(cpu_index())
}

/// The syscall path on this core acts on record `i` from now on.
pub fn set_task_slot(i: usize) {
    TASKS.set_current(cpu_index(), i);
}

/// Run `f` on record `slot`.
pub fn with_task_slot<R>(slot: usize, f: impl FnOnce(&mut Task) -> R) -> R {
    TASKS.with_slot(slot, f)
}

/// Run `f` on the record the syscall path acts on now.
pub fn with_task<R>(f: impl FnOnce(&mut Task) -> R) -> R {
    with_task_slot(task_slot_index(), f)
}

/// The page-table root of the record the syscall path acts on now.
#[must_use]
pub fn task_root() -> u64 {
    TASKS.root_of(task_slot_index())
}

/// The page-table root of record `i` (`0`: none, the boot root).
#[must_use]
pub fn record_root(i: usize) -> u64 {
    TASKS.root_of(i)
}

/// The aarch64 machine.
pub struct Aarch64;

/// `akuma-phys`'s fresh frame, as the core's.
pub struct Frame(FreshFrame);

impl FreshPage for Frame {
    fn alloc() -> Option<Self> {
        FreshFrame::alloc().map(Self)
    }
    fn alloc_zeroed() -> Option<Self> {
        FreshFrame::alloc_zeroed().map(Self)
    }
    fn pa(&self) -> usize {
        self.0.pa()
    }
    fn bytes_mut(&mut self) -> &mut [u8; 4096] {
        self.0.bytes_mut()
    }
    fn copy_from(&mut self, src_pa: usize) {
        self.0.copy_from_frame(src_pa);
    }
    fn publish(self) -> usize {
        self.0.publish()
    }
}

/// A zeroed frame for a page table, given up to the table that links it.
fn table_frame() -> Option<u64> {
    FreshFrame::alloc_zeroed().map(|f| f.publish() as u64)
}

/// Make a new table or leaf visible to the walker before a translation can use it.
fn publish_tables() {
    akuma_cpu::barrier::dsb_ishst();
}

impl UserPages for Aarch64 {
    type Frame = Frame;

    fn leaf(root: u64, va: u64) -> Option<u64> {
        walk::leaf_slot::<Physmap>(root, va as usize).map(|s| s.get())
    }

    fn write_leaf(root: u64, va: u64, entry: u64) -> bool {
        walk::leaf_slot::<Physmap>(root, va as usize).map(|s| s.set(entry)).is_some()
    }

    fn set_leaf(root: u64, va: u64, entry: u64) -> bool {
        let ok = walk::map_leaf::<Physmap>(root, va as usize, entry, table_frame);
        publish_tables();
        ok
    }

    fn map_user(root: u64, va: u64, pa: u64, write: bool, exec: bool) -> bool {
        Self::set_leaf(root, va, pte::user_leaf(pa, write, exec, true))
    }

    fn invalidate(va: u64) {
        akuma_cpu::barrier::dsb_ishst();
        akuma_cpu::tlb::vaae1(va >> 12);
        akuma_cpu::barrier::dsb_ish();
        akuma_cpu::barrier::isb();
    }

    fn writable(e: u64) -> bool {
        pte::writable(e)
    }

    fn cow_marked(e: u64) -> bool {
        pte::cow_marked(e)
    }

    fn written(e: u64) -> bool {
        pte::written(e)
    }

    fn with_written(e: u64) -> u64 {
        pte::with_written(e)
    }

    fn take(e: u64) -> u64 {
        pte::take(e)
    }

    fn take_at(e: u64, pa: u64) -> u64 {
        pte::take_at(e, pa)
    }

    fn share_marked(e: u64) -> u64 {
        pte::share_marked(e)
    }

    /// A page EL0 cannot reach is a `PROT_NONE` one, and stays inaccessible.
    fn anon_like(e: u64) -> Lazy {
        let write = pte::writable(e) || pte::cow_marked(e);
        Lazy { none: !pte::user(e), ..Lazy::anon(write, pte::exec(e)) }
    }
}

impl akuma_tokio_kernel_core::mm::Mm for Aarch64 {
    fn with_task<R>(f: impl FnOnce(&mut Task) -> R) -> R {
        with_task(f)
    }
    fn translate(root: u64, va: u64) -> Option<u64> {
        walk::translate::<Physmap>(root, va as usize)
    }
    fn unmap(root: u64, va: u64) -> Option<u64> {
        walk::unmap::<Physmap>(root, va as usize)
    }
    fn map_page(root: u64, va: u64, pa: u64, write: bool, exec: bool, user: bool) -> bool {
        Self::set_leaf(root, va, pte::user_leaf(pa, write, exec, user))
    }
    fn frame_of(e: u64) -> Option<u64> {
        pte::frame(e)
    }
    /// One core, every task under ASID 0: drop this core's translations.
    fn shootdown() {
        akuma_cpu::barrier::dsb_ishst();
        akuma_cpu::tlb::vmalle1();
        akuma_cpu::barrier::dsb_ish();
        akuma_cpu::barrier::isb();
    }
    fn mmdbg() -> u32 {
        7
    }
    fn mm_event(_kind: u8, _start: u64, _end: u64) {}
}

impl akuma_tokio_kernel_core::lazy::Kernel for Aarch64 {
    fn root_fs() -> &'static dyn akuma_vfs::Filesystem {
        crate::fs::root_fs()
    }
    fn task_root() -> u64 {
        task_root()
    }
}

impl akuma_tokio_kernel_core::tree::UserTree for Aarch64 {
    const LAYOUT: akuma_user_tree::PteLayout = akuma_user_tree::AARCH64;
    fn entry(table: u64, i: usize) -> u64 {
        akuma_pagetable::entry::<Physmap>(table, i)
    }
    fn set_entry(table: u64, i: usize, value: u64) {
        akuma_pagetable::set_entry::<Physmap>(table, i, value);
    }
    fn user_root_slot(i: usize) -> bool {
        akuma_boot_aarch64::user::user_root_slot(i)
    }
    fn kernel_entry(depth: u32, va: u64) -> bool {
        akuma_boot_aarch64::user::kernel_entry(depth, va)
    }
    fn install_kernel(dst: u64) -> bool {
        let ok = akuma_boot_aarch64::user::install_kernel(dst, table_frame);
        publish_tables();
        ok
    }
    fn live_root() -> u64 {
        akuma_boot_aarch64::user::live_root()
    }
}

/// Boot check of this machine's memory code, end to end: a fresh root, a user
/// page mapped into it, `TTBR0` switched to it (the kernel keeps running: the
/// root carries its part), the page read back through its user address, a
/// copy-on-write clone of the root, then both freed and the PMM count checked
/// to be what it was. `false` names the first step that failed, on the console.
pub fn self_test() -> bool {
    use akuma_primitives::safe_print;
    use akuma_tokio_kernel_core::tree;
    const VA: u64 = 0x40_0000;
    // The boot thread acts on record 0 (the clone reads its shared spans).
    set_task_slot(0);
    let before = akuma_pmm::free_count();
    let Some(root) = tree::new_root::<Aarch64>() else {
        safe_print!(64, "  mm: no fresh root\n");
        return false;
    };
    let Some(mut page) = FreshFrame::alloc_zeroed() else { return false };
    page.bytes_mut()[..8].copy_from_slice(&0x5a5a_1234_u64.to_le_bytes());
    let pa = page.publish() as u64;
    if !<Aarch64 as UserPages>::map_user(root, VA, pa, true, false) {
        safe_print!(64, "  mm: map_user failed\n");
        return false;
    }
    if !akuma_boot_aarch64::user::switch_to(root) {
        safe_print!(64, "  mm: the root lacks the kernel's part\n");
        return false;
    }
    let mut word = [0u8; 8];
    let read = akuma_user_access::copy_from_user(&mut word, VA).is_ok();
    let _ = akuma_boot_aarch64::user::switch_to(0);
    if !read || u64::from_le_bytes(word) != 0x5a5a_1234 {
        safe_print!(96, "  mm: user read through TTBR0 gave ok={} {:#x}\n", read, u64::from_le_bytes(word));
        return false;
    }
    let Some(child) = <Frame as FreshPage>::alloc_zeroed().map(|f| f.publish() as u64) else { return false };
    if tree::clone_user_half::<Aarch64>(root, child).is_none() {
        safe_print!(64, "  mm: clone failed\n");
        return false;
    }
    let shared = <Aarch64 as UserPages>::leaf(child, VA).is_some_and(|e| pte::cow_marked(e) && !pte::writable(e));
    let freed = tree::free_user_root::<Aarch64>(child) + tree::free_user_root::<Aarch64>(root);
    let after = akuma_pmm::free_count();
    safe_print!(
        128,
        "  mm: root+page+clone ok, cow-shared={}, {} frames freed, pmm {} -> {}\n",
        shared,
        freed,
        before,
        after
    );
    shared && after == before
}

/// `AT_HWCAP`: FP and Advanced SIMD, which every core this runs on has.
/// The optional extensions are not claimed until they are read from
/// `ID_AA64ISAR0_EL1`; a program then takes its portable path.
impl akuma_tokio_kernel_core::loader::Loader for Aarch64 {
    const HWCAP: u64 = (1 << 0) | (1 << 1);
}

impl akuma_tokio_kernel_core::proc::Proc for Aarch64 {
    fn current_slot() -> usize {
        crate::sched::current_slot()
    }
    fn prepare_slot(slot: usize) -> bool {
        crate::sched::prepare(slot).is_some()
    }
    /// No kernel lock yet (one core, IRQs masked in the kernel): just run it.
    fn unlocked(f: impl FnOnce()) {
        f();
    }
    fn uptime_us() -> u64 {
        crate::fs::uptime_us()
    }
    fn task_slot_index() -> usize {
        task_slot_index()
    }
    fn with_task_slot<R>(i: usize, f: impl FnOnce(&mut Task) -> R) -> R {
        with_task_slot(i, f)
    }
    fn record_root(i: usize) -> u64 {
        record_root(i)
    }
    fn copy_to_user(dst_va: u64, src: &[u8]) -> bool {
        akuma_user_access::copy_to_user(dst_va, src).is_ok()
    }
}

//! AP bring-up plumbing that belongs to `boot.s`: the trampoline page, its mailbox, and the boot root the
//! started core enables paging on.
//!
//! Both amd64 kernels (`akuma-amd64` and `akuma-tokio-kernel-amd64`) used to carry byte-identical copies of
//! this: the `AP_*` constants mirrored from `boot.s`, the three `extern` symbols it defines, and the three
//! functions below. They live here because this crate instantiates `boot.s`, so the contract with its symbols
//! is stated once, next to the assembly that depends on it.
//!
//! What stays with each kernel: the policy (which cores, in what order, with what stacks), the INIT/STARTUP
//! sequence and the wait for the AP to report in.

use crate::phys::phys_ptr;

/// Where the trampoline is copied. A STARTUP IPI's vector is the page number of a 4 KiB page below 1 MiB
/// (`0x08` -> `0x8000`); this one is clear of every boot structure both VMMs place (QEMU's `hvm_start_info`
/// at `0x1580`, Firecracker's at `0x6000` with its memory map at `0x7000` and command line at `0x20000`) and
/// of the PMM, which is handed the region *containing the kernel* (from 1 MiB) and never the low one.
pub const AP_TRAMPOLINE_PA: u64 = 0x8000;
/// The STARTUP IPI vector for [`AP_TRAMPOLINE_PA`].
pub const AP_STARTUP_VECTOR: u8 = (AP_TRAMPOLINE_PA >> 12) as u8;

/// The mailbox the trampoline reads: fixed offsets inside the trampoline page, mirrored as `AP_MB_*` in `boot.s`.
pub const AP_MB_CR3: u64 = AP_TRAMPOLINE_PA + 0xF00;
/// Mailbox: the AP's initial stack pointer.
pub const AP_MB_STACK: u64 = AP_TRAMPOLINE_PA + 0xF08;
/// Mailbox: the 64-bit entry the trampoline jumps to.
pub const AP_MB_ENTRY: u64 = AP_TRAMPOLINE_PA + 0xF10;
/// Mailbox: the core's index, passed to the entry in a register.
pub const AP_MB_INDEX: u64 = AP_TRAMPOLINE_PA + 0xF18;

/// Present + writable, for the slot-0 entry of the AP boot root.
const PTE_P_RW: u64 = 0x3;

unsafe extern "C" {
    /// First byte of the trampoline blob (`boot.s`, in `.rodata`).
    static ap_trampoline_start: u8;
    /// One past the last byte of the trampoline blob.
    static ap_trampoline_end: u8;
    /// The AP boot root, a page in the low boot region.
    static __ap_pml4: u8;
    /// The identity map `boot.s` built, which `drop_identity_map` unlinked from the kernel root but did not destroy.
    static __pdpt_low: u8;
}

/// Build the page-table root an AP enables paging on and return its **physical** address: the kernel's PML4
/// with slot 0 pointing back at `boot.s`'s identity map, so the trampoline has somewhere to stand before it
/// jumps high.
///
/// It is a static page and not a PMM frame because the trampoline loads `CR3` in 32-bit protected mode, where
/// it is 32 bits wide: a root above 4 GiB is truncated and the core triple-faults on its first paged
/// instruction. `__ap_pml4` is linked low (VMA = LMA), so its address is physical and below 4 GiB.
///
/// `kernel_root` must be the live kernel PML4 (a page-table frame inside the physmap). Passing anything else
/// builds a garbage AP root and the AP never comes up; it is a logic error, not a memory-safety one, since the
/// copy goes through the physmap's bounds check and only ever writes the static `__ap_pml4` page.
///
/// # Panics
/// If `kernel_root` is outside the physmap.
#[must_use]
pub fn ap_boot_root(kernel_root: u64) -> u64 {
    let root = (&raw const __ap_pml4) as u64;
    let identity = (&raw const __pdpt_low) as u64;
    // SAFETY: both symbols are page-aligned pages in `.bss.pagetables`, whose VMA is its LMA, so their addresses
    // are physical and inside the physmap. `kernel_root` is bounds-checked by `phys_ptr` (a read of one page);
    // the only destination written is the static `__ap_pml4` page, which nothing else uses.
    unsafe {
        core::ptr::copy_nonoverlapping(phys_ptr::<u8>(kernel_root), phys_ptr::<u8>(root), 4096);
        phys_ptr::<u64>(root).write_volatile(identity | PTE_P_RW);
    }
    root
}

/// Copy the trampoline to its page. Returns `false` if it does not fit below the mailbox, which is a build error
/// surfacing at run time.
///
/// # Safety
/// The page at [`AP_TRAMPOLINE_PA`] must be ordinary RAM that belongs to no one: the caller has checked that the
/// machine's map and the loader's own structures leave it free ([`TrampolinePage::claim`], or
/// the fixed layouts of the two VMMs named at [`AP_TRAMPOLINE_PA`]). This overwrites up to 3.75 KiB of it.
pub unsafe fn install_trampoline() -> bool {
    // Two linker-visible symbols bracketing a `.rodata` blob in the kernel image; the difference is its length.
    let (src, len) = {
        let s = &raw const ap_trampoline_start;
        let e = &raw const ap_trampoline_end;
        (s, e as usize - s as usize)
    };
    if len > (AP_MB_CR3 - AP_TRAMPOLINE_PA) as usize {
        return false;
    }
    // SAFETY: the destination page is RAM inside the physmap, owned by bring-up (the caller's contract), and the
    // blob is `len` bytes of kernel image that fit below the mailbox.
    unsafe { core::ptr::copy_nonoverlapping(src, phys_ptr::<u8>(AP_TRAMPOLINE_PA), len) };
    true
}

/// The trampoline page, proven free: made only by [`TrampolinePage::claim`], so holding one means the check
/// that makes [`install_trampoline`] sound has passed.
#[derive(Clone, Copy, Debug)]
pub struct TrampolinePage(());

impl TrampolinePage {
    /// The page at [`AP_TRAMPOLINE_PA`], if it is ordinary RAM and clear of everything the boot loader placed.
    ///
    /// [`AP_TRAMPOLINE_PA`] is a claim about two VMMs' layouts. A firmware boot is a different machine: UEFI's
    /// map fragments low memory, and GRUB puts its information block wherever it likes. Copying the trampoline
    /// over either would corrupt something the kernel is still reading - so the caller names what it placed
    /// (`keep_out`, as `[start, end)` byte ranges) and this refuses unless the page is inside a RAM region and
    /// outside all of them. The page is below the 1 MiB floor the page allocator starts at, so nothing
    /// allocates it later. A refusal means "stay single-core", which is a configuration, not a failure.
    #[must_use]
    pub fn claim(facts: &akuma_machine_facts::Facts, keep_out: &[(u64, u64)]) -> Option<Self> {
        use akuma_machine_facts::Range;
        // The decision is the neutral one (`Facts::is_clear_ram`); this only names the page and converts the
        // caller's `[start, end)` pairs.
        facts
            .is_clear_ram(
                Range::new(AP_TRAMPOLINE_PA, 4096),
                keep_out.iter().map(|&(s, e)| Range::new(s, e.saturating_sub(s))),
            )
            .then_some(Self(()))
    }

    /// Copy the trampoline to the page. `false` if it does not fit below the mailbox. Bring-up is serial
    /// (one core at a time through one mailbox), so no AP is executing the blob while it is rewritten.
    #[must_use]
    pub fn install(self) -> bool {
        // SAFETY: `claim` proved the page is RAM no one else owns (its contract above).
        unsafe { install_trampoline() }
    }
}

/// Copy the trampoline to its page, **once**, for the two VMM layouts named at [`AP_TRAMPOLINE_PA`].
///
/// The safe form of [`install_trampoline`]. The claim that makes the copy sound - the page at
/// [`AP_TRAMPOLINE_PA`] is RAM no one else owns - is a property of that constant (the fixed layouts of the
/// two VMMs this kernel runs under, documented where the constant is defined), so it is stated here beside
/// the constant instead of at each caller. A second call changes nothing and returns `true`: the blob is
/// already there and the APs may be reading it.
///
/// Returns `false` if the blob does not fit below the mailbox.
pub fn install_trampoline_once() -> bool {
    use core::sync::atomic::{AtomicBool, Ordering};
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::AcqRel) {
        return true;
    }
    // SAFETY: the page is RAM below 1 MiB, clear of the PVH block and owned by AP bring-up for its duration
    // (the layouts named at `AP_TRAMPOLINE_PA`); the once-guard above keeps a second copy from running
    // under an AP that is already executing the blob.
    unsafe { install_trampoline() }
}

/// Write one mailbox word the trampoline will read. `pa` must be one of the `AP_MB_*` addresses.
///
/// # Panics
/// If `pa` is not a mailbox slot: the write is confined to the trampoline page's mailbox by construction.
pub fn mailbox_write(pa: u64, v: u64) {
    assert!(
        matches!(pa, AP_MB_CR3 | AP_MB_STACK | AP_MB_ENTRY | AP_MB_INDEX),
        "mailbox_write outside the trampoline mailbox"
    );
    // SAFETY: `pa` is one of the four aligned 8-byte mailbox slots inside the trampoline page, which bring-up
    // owns (`install_trampoline`'s contract); the physmap bounds check covers the address.
    unsafe { phys_ptr::<u64>(pa).write_volatile(v) };
}

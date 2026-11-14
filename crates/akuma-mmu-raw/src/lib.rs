//! The raw operations behind `akuma-mmu`'s safe page-table logic. `unsafe` allowed here; `akuma-mmu` forbids it.
//!
//! Every function states the fact its caller owns. Where a function can check part of that fact it does, and
//! says so: the PMM range (`akuma_pmm::contains`) for frames the caller allocated, the RAM physmap window for
//! frames a page-table walk just resolved. What cannot be checked here - that a user VA is mapped, that a
//! TTBR0 maps the kernel - is passed in as a `bool` the caller computed, so the claim is spelled at the call
//! site rather than hidden in a block.

#![no_std]
#![allow(clippy::cast_ptr_alignment)]

use akuma_primitives::addr::phys_to_virt;

/// Page size.
pub const PAGE_SIZE: usize = 4096;

// ── physmap copies ──────────────────────────────────────────────────────────────────────────────────────

/// Copy `dst.len()` bytes out of the PMM-managed physical range at `pa`. `false` if the range is not PMM RAM.
#[must_use]
pub fn copy_from_phys(pa: usize, dst: &mut [u8]) -> bool {
    if dst.is_empty() {
        return true;
    }
    if !akuma_pmm::contains(pa, dst.len()) {
        return false;
    }
    // SAFETY: the whole range is inside PMM-managed RAM, identity-mapped and readable; `dst` is a live
    // exclusive slice and cannot overlap it (kernel heap or stack, never a raw frame view).
    unsafe { core::ptr::copy_nonoverlapping(phys_to_virt(pa).cast_const(), dst.as_mut_ptr(), dst.len()) };
    true
}

/// Copy `src` into the PMM-managed physical range at `pa`. `false` if the range is not PMM RAM.
#[must_use]
pub fn copy_to_phys(pa: usize, src: &[u8]) -> bool {
    if src.is_empty() {
        return true;
    }
    if !akuma_pmm::contains(pa, src.len()) {
        return false;
    }
    // SAFETY: as `copy_from_phys`, roles reversed.
    unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), phys_to_virt(pa), src.len()) };
    true
}

/// Copy one frame `src` -> `dst`. `false` if either is not a page-aligned PMM frame or they are the same frame.
#[must_use]
pub fn copy_page(dst: usize, src: usize) -> bool {
    if !dst.is_multiple_of(PAGE_SIZE)
        || !src.is_multiple_of(PAGE_SIZE)
        || dst == src
        || !akuma_pmm::contains(dst, PAGE_SIZE)
        || !akuma_pmm::contains(src, PAGE_SIZE)
    {
        return false;
    }
    // SAFETY: both ranges are page-aligned, `PAGE_SIZE` long, inside PMM RAM and - checked above - disjoint.
    unsafe { core::ptr::copy_nonoverlapping(phys_to_virt(src).cast_const(), phys_to_virt(dst), PAGE_SIZE) };
    true
}

/// Write a plain `Copy` value into the PMM frame at `pa` (a frame base: the write is `align_of::<T>()`-aligned
/// only if `pa` is). `false` if the bytes leave PMM RAM or `pa` is misaligned for `T`.
#[must_use]
pub fn write_phys<T: Copy>(pa: usize, val: &T) -> bool {
    if !pa.is_multiple_of(core::mem::align_of::<T>()) || !akuma_pmm::contains(pa, core::mem::size_of::<T>()) {
        return false;
    }
    // SAFETY: `pa` holds `size_of::<T>()` bytes of PMM RAM (identity-mapped, writable), aligned for `T`.
    unsafe { core::ptr::write(phys_to_virt(pa).cast::<T>(), *val) };
    true
}

/// Run `f` on a `&mut [u8]` window `[offset, offset + len)` of the page-aligned PMM frame at `pa`. `None` if
/// `pa` is not a page-aligned PMM frame or the window leaves the page.
///
/// The caller owns the fact that the frame is not published to any page table, so `f` holds the only
/// reference for its duration.
pub fn with_phys_window<R>(pa: usize, offset: usize, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> Option<R> {
    if !pa.is_multiple_of(PAGE_SIZE) || offset.checked_add(len)? > PAGE_SIZE || !akuma_pmm::contains(pa, PAGE_SIZE) {
        return None;
    }
    // SAFETY: a page-aligned PMM frame (identity-mapped, writable); the window is inside the page; see the doc
    // for the exclusivity the caller owns.
    let buf = unsafe { core::slice::from_raw_parts_mut(phys_to_virt(pa).add(offset), len) };
    Some(f(buf))
}

/// The window check for a frame a page-table walk just resolved (not necessarily PMM-managed): on the bare-metal
/// target the range must lie inside the RAM physmap.
fn mapped_range_ok(pa: usize, len: usize) -> bool {
    #[cfg(target_os = "none")]
    {
        let Some(end) = pa.checked_add(len) else { return false };
        len != 0 && akuma_primitives::addr::physmap_contains(pa) && akuma_primitives::addr::physmap_contains(end - 1)
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (pa, len);
        true
    }
}

/// Copy `bytes` to physical `pa`, a frame a walk just resolved. `false` if the range is outside RAM. The caller
/// owns exclusivity (`&mut` on its address space) and keeps the write inside the one frame.
#[must_use]
pub fn write_resolved(pa: usize, bytes: &[u8]) -> bool {
    if !mapped_range_ok(pa, bytes.len()) {
        return false;
    }
    // SAFETY: the range is inside RAM (checked); exclusivity and the one-frame bound are the caller's.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), phys_to_virt(pa), bytes.len()) };
    true
}

/// Zero one whole page of the frame a walk just resolved at `pa`. `false` if outside RAM.
#[must_use]
pub fn zero_resolved_page(pa: usize) -> bool {
    if !mapped_range_ok(pa, PAGE_SIZE) {
        return false;
    }
    // SAFETY: a whole page inside RAM (checked) that the caller's walk resolved.
    unsafe { core::ptr::write_bytes(phys_to_virt(pa), 0, PAGE_SIZE) };
    true
}

// ── page tables ─────────────────────────────────────────────────────────────────────────────────────────

/// A page-table reference at the kernel VA `addr` of a table frame.
///
/// The caller owns the fact that `addr` is the physmap address of a live, 4 KiB-aligned table frame that
/// outlives the reference (frames are only freed after a TLB-safe grace period). The entries are atomics, so a
/// shared reference stays sound while the MMU walker reads them.
///
/// # Panics
/// If `addr` is not 4 KiB-aligned.
#[allow(clippy::inline_always)] // the page-walk hot path: it was `inline(always)` in `akuma-mmu`
#[inline(always)]
#[must_use]
pub fn table_at(addr: usize) -> &'static akuma_pagetable::Table {
    assert!(addr.is_multiple_of(PAGE_SIZE), "table_at: unaligned table address {addr:#x}");
    // SAFETY: the contract above.
    unsafe { akuma_pagetable::Table::at(addr) }
}

// ── user VA access from EL1 ─────────────────────────────────────────────────────────────────────────────

/// Read a `Copy` POD `T` at user `va` in the current address space. `mapped` is the caller's own check that
/// `[va, va + size_of::<T>())` is mapped and EL0-accessible in the live tables; `None` if it is false.
#[must_use]
pub fn read_user<T: Copy>(va: usize, mapped: bool) -> Option<T> {
    if !mapped {
        return None;
    }
    // SAFETY: `mapped` states the range is mapped and EL0-accessible in the live tables, so this EL1 read cannot
    // fault; `read_unaligned` drops the alignment obligation; `T: Copy` is POD.
    Some(unsafe { core::ptr::read_unaligned(va as *const T) })
}

/// Write a `Copy` POD `T` to user `va`. `mapped_writable` is the caller's check that the range is mapped
/// writable and EL0-accessible; `false` (nothing written) if it is false.
#[must_use]
pub fn write_user<T: Copy>(va: usize, val: &T, mapped_writable: bool) -> bool {
    if !mapped_writable {
        return false;
    }
    // SAFETY: as `read_user`, for a store.
    unsafe { core::ptr::write_unaligned(va as *mut T, *val) };
    true
}

// ── x86_64 CR3 ──────────────────────────────────────────────────────────────────────────────────────────

/// Load `root` into CR3. Refuses (panics) a root that is zero, unaligned or beyond the 52-bit physical space.
///
/// The caller owns the fact that `root` is a PML4 mapping everything the kernel executes from and touches
/// before switching back: its own address space (which aliases the kernel half by construction) or the boot
/// root captured before any other existed.
///
/// # Panics
/// If `root` is not a page-aligned physical address.
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
pub fn write_cr3(root: u64) {
    assert!(root != 0 && root.trailing_zeros() >= 12 && root >> 52 == 0, "write_cr3: bad root {root:#x}");
    // SAFETY: the contract above; the page-alignment checks are what a function can verify.
    unsafe { akuma_pagetable::amd64::load_cr3(root) };
}

// ── aarch64 boot and TTBR0 ──────────────────────────────────────────────────────────────────────────────

/// The boot TTBR0 value stored in `boot_ttbr0_addr` (a symbol the boot code defines).
#[cfg(all(target_os = "none", target_arch = "aarch64"))]
#[must_use]
pub fn boot_ttbr0_addr() -> u64 {
    let addr: u64;
    // SAFETY: `boot_ttbr0_addr` is a boot-code-defined 8-byte symbol, read PC-relative once.
    unsafe {
        core::arch::asm!(
            "adrp {tmp}, boot_ttbr0_addr",
            "add {tmp}, {tmp}, :lo12:boot_ttbr0_addr",
            "ldr {out}, [{tmp}]",
            tmp = out(reg) _,
            out = out(reg) addr,
        );
    }
    addr
}

/// `dsb ish; msr ttbr0_el1; isb` with `ttbr0` (no TLB flush: the caller's publish brackets and flush policy
/// own that). `ttbr0` must map the running kernel; refuses the one value that can never be a root.
///
/// # Panics
/// If `ttbr0` has no table address.
#[cfg(all(target_os = "none", target_arch = "aarch64"))]
pub fn install_ttbr0(ttbr0: u64) {
    assert!(ttbr0 & 0x0000_FFFF_FFFF_F000 != 0, "install_ttbr0: no table address in {ttbr0:#x}");
    // SAFETY: the one `msr ttbr0_el1` in `akuma-mmu`; the obligation is the caller's, stated above.
    unsafe { core::arch::asm!("dsb ish", "msr ttbr0_el1, {ttbr0}", "isb", ttbr0 = in(reg) ttbr0) };
}

/// Read the flattened device tree at `base` for one closure. `base` must be mapped (the caller extends the
/// boot identity map first); `locate` validates the FDT magic and size before trusting either.
#[cfg(all(target_os = "none", target_arch = "aarch64"))]
pub fn with_dtb<R>(base: usize, f: impl FnOnce(Option<&akuma_fdt::Dtb<'_>>) -> R) -> R {
    // SAFETY: `base` is confirmed mapped through the boot identity map by the caller; boot is single-threaded
    // on the boot page table before any user address space exists.
    let dtb = unsafe { akuma_fdt::locate(base) };
    f(dtb.as_ref())
}

/// Section bounds from the linker script.
#[derive(Clone, Copy, Debug)]
pub struct KernelSections {
    pub text_start: usize,
    pub text_end: usize,
    pub rodata_start: usize,
    pub rodata_end: usize,
    pub data_start: usize,
}

/// The kernel image's section addresses (linker symbols; only their addresses are taken).
#[must_use]
pub fn kernel_sections() -> KernelSections {
    unsafe extern "C" {
        static _text_start: u8;
        static _text_end: u8;
        static _rodata_start: u8;
        static _rodata_end: u8;
        static _data_start: u8;
    }
    // Taking the address of a linker-defined symbol reads nothing.
    KernelSections {
        text_start: &raw const _text_start as usize,
        text_end: &raw const _text_end as usize,
        rodata_start: &raw const _rodata_start as usize,
        rodata_end: &raw const _rodata_end as usize,
        data_start: &raw const _data_start as usize,
    }
}

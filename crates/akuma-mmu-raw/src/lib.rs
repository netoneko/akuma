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
//
// Every copy goes through `akuma_phys::Frame`, whose methods are bounds-checked and safe once a frame is named.
// Naming one is the claim `Frame::from_pa` states - RAM that no Rust object lives in - and it is made in exactly
// two places below, one per kind of frame this crate is handed: a PMM frame the caller allocated
// (`pmm_frame`), and a frame a page-table walk just resolved (`resolved_frame`).

use akuma_phys::{Frame, PhysAddr};

/// The PMM frame containing `pa`, when `[pa, pa + len)` lies in PMM-managed RAM. `None` otherwise.
fn pmm_frame(pa: usize, len: usize) -> Option<Frame> {
    if !akuma_pmm::contains(pa, len.max(1)) {
        return None;
    }
    // SAFETY: the crate contract for PMM addresses - the caller passes frames it allocated from the PMM and holds
    // as raw data (a user page, a page-table frame, a staged image), never a frame a Rust object lives in - and
    // the range is checked to be PMM RAM, which the physmap covers.
    Some(unsafe { Frame::from_pa(PhysAddr::from_usize(pa & !(PAGE_SIZE - 1))) })
}

/// The frame containing `pa`, a frame a page-table walk just resolved, when `[pa, pa + len)` is in RAM.
fn resolved_frame(pa: usize, len: usize) -> Option<Frame> {
    if !mapped_range_ok(pa, len) {
        return None;
    }
    // SAFETY: the crate contract for resolved frames - a present leaf of a user address space the caller holds
    // `&mut` on names a user page, which is raw data and no Rust object - and the range is checked to be RAM.
    Some(unsafe { Frame::from_pa(PhysAddr::from_usize(pa & !(PAGE_SIZE - 1))) })
}

/// Split `[pa, pa + len)` at page boundaries: `(page-aligned pa, offset in page, chunk length, offset in range)`.
fn page_chunks(pa: usize, len: usize) -> impl Iterator<Item = (usize, usize, usize, usize)> {
    let mut done = 0;
    core::iter::from_fn(move || {
        if done >= len {
            return None;
        }
        let at = pa + done;
        let off = at % PAGE_SIZE;
        let n = (PAGE_SIZE - off).min(len - done);
        let chunk = (at - off, off, n, done);
        done += n;
        Some(chunk)
    })
}

/// Copy `dst.len()` bytes out of the PMM-managed physical range at `pa`. `false` if the range is not PMM RAM.
#[must_use]
pub fn copy_from_phys(pa: usize, dst: &mut [u8]) -> bool {
    if dst.is_empty() {
        return true;
    }
    if pmm_frame(pa, dst.len()).is_none() {
        return false;
    }
    for (page, off, n, at) in page_chunks(pa, dst.len()) {
        let Some(frame) = pmm_frame(page, PAGE_SIZE) else { return false };
        frame.read_bytes(off, &mut dst[at..at + n]);
    }
    true
}

/// Copy `src` into the PMM-managed physical range at `pa`. `false` if the range is not PMM RAM.
#[must_use]
pub fn copy_to_phys(pa: usize, src: &[u8]) -> bool {
    if src.is_empty() {
        return true;
    }
    if pmm_frame(pa, src.len()).is_none() {
        return false;
    }
    for (page, off, n, at) in page_chunks(pa, src.len()) {
        let Some(frame) = pmm_frame(page, PAGE_SIZE) else { return false };
        frame.write_bytes(off, &src[at..at + n]);
    }
    true
}

/// Copy one frame `src` -> `dst`. `false` if either is not a page-aligned PMM frame or they are the same frame.
#[must_use]
pub fn copy_page(dst: usize, src: usize) -> bool {
    if !dst.is_multiple_of(PAGE_SIZE) || !src.is_multiple_of(PAGE_SIZE) || dst == src {
        return false;
    }
    let (Some(d), Some(s)) = (pmm_frame(dst, PAGE_SIZE), pmm_frame(src, PAGE_SIZE)) else { return false };
    d.copy_page_from(s);
    true
}

/// Write a [`Pod`](akuma_pod::Pod) value into the PMM frame at `pa` (a frame base: the write is `align_of::<T>()`-aligned
/// only if `pa` is). `false` if the bytes leave PMM RAM or `pa` is misaligned for `T`.
#[must_use]
pub fn write_phys<T: akuma_pod::Pod>(pa: usize, val: &T) -> bool {
    pa.is_multiple_of(core::mem::align_of::<T>()) && copy_to_phys(pa, akuma_pod::bytes_of(val))
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
///
/// # Panics
/// If the write crosses the end of `pa`'s frame (the caller's one-frame bound).
#[must_use]
pub fn write_resolved(pa: usize, bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return mapped_range_ok(pa, 0);
    }
    let Some(frame) = resolved_frame(pa, bytes.len()) else { return false };
    frame.write_bytes(pa % PAGE_SIZE, bytes);
    true
}

/// Zero one whole page of the frame a walk just resolved at `pa`. `false` if outside RAM.
#[must_use]
pub fn zero_resolved_page(pa: usize) -> bool {
    /// Zeros copied a slice at a time: a 512-byte constant rather than a page of stack.
    const ZEROS: [u8; 512] = [0; 512];
    let Some(frame) = resolved_frame(pa, PAGE_SIZE) else { return false };
    for off in (0..PAGE_SIZE).step_by(ZEROS.len()) {
        frame.write_bytes(off, &ZEROS);
    }
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

/// Read a [`Pod`](akuma_pod::Pod) `T` at user `va` in the current address space. `mapped` is the caller's own check that
/// `[va, va + size_of::<T>())` is mapped and EL0-accessible in the live tables; `None` if it is false.
#[must_use]
pub fn read_user<T: akuma_pod::Pod>(va: usize, mapped: bool) -> Option<T> {
    if !mapped {
        return None;
    }
    // SAFETY: `mapped` states the range is mapped and EL0-accessible in the live tables, so this EL1 read cannot
    // fault; `read_unaligned` drops the alignment obligation; `T: Pod` makes any bytes user space left there a
    // valid `T`.
    Some(unsafe { core::ptr::read_unaligned(va as *const T) })
}

/// Write a [`Pod`](akuma_pod::Pod) `T` (no padding: no kernel bytes leak) to user `va`. `mapped_writable` is the caller's check that the range is mapped
/// writable and EL0-accessible; `false` (nothing written) if it is false.
#[must_use]
pub fn write_user<T: akuma_pod::Pod>(va: usize, val: &T, mapped_writable: bool) -> bool {
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

#[cfg(test)]
mod tests {
    use super::{PAGE_SIZE, page_chunks};

    extern crate std;
    use std::vec::Vec;

    #[test]
    fn page_chunks_split_at_every_page_boundary() {
        let chunks: Vec<_> = page_chunks(PAGE_SIZE - 8, 8 + PAGE_SIZE + 3).collect();
        assert_eq!(
            chunks,
            [(0, PAGE_SIZE - 8, 8, 0), (PAGE_SIZE, 0, PAGE_SIZE, 8), (2 * PAGE_SIZE, 0, 3, 8 + PAGE_SIZE)]
        );
        assert_eq!(page_chunks(2 * PAGE_SIZE + 5, 10).collect::<Vec<_>>(), [(2 * PAGE_SIZE, 5, 10, 0)]);
        assert_eq!(page_chunks(0, 0).count(), 0);
    }
}

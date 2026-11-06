//! Page frames through the kernel's physmap: [`Frame`], a shared view of a frame
//! something else owns (a page-table frame, a mapped user page), and
//! [`FreshFrame`], a frame fresh from the PMM that this code owns alone until it
//! publishes it.
//!
//! Architecture-neutral: the physmap is `akuma_primitives::addr`'s, at
//! `0xFFFF_8000_0000_0000 + pa` on amd64 and the identity on AArch64. Moved
//! from `akuma-boot-amd64::phys` (B21 A5.5) so the aarch64 tokio kernel uses the
//! same two types instead of a second copy; `akuma-boot-amd64::phys` re-exports
//! them, so no amd64 call site changed.
//!
//! # The one stated contract
//!
//! An address handed to [`Frame::from_pa`] is a frame of RAM that no Rust
//! object lives in and that the physmap reaches. [`FreshFrame`] needs no
//! such promise: the PMM handed it over and nothing else names it.

#![no_std]

extern crate alloc;

pub use akuma_primitives::addr::PhysAddr;

/// Bytes in a frame.
const PAGE_SIZE: usize = 4096;

/// The kernel-virtual pointer for physical address `pa`.
///
/// # Panics
/// If `pa` is outside the physmap: a plausible-looking pointer into nothing is
/// worse than a named stop.
#[must_use]
#[inline]
pub fn phys_ptr<T>(pa: PhysAddr) -> *mut T {
    assert!(akuma_primitives::addr::physmap_contains(pa.as_usize()), "physical address outside the physmap");
    akuma_primitives::addr::phys_to_virt(pa.as_usize()).cast::<T>()
}

/// Copy `out.len()` bytes of RAM at physical address `pa` into `out`, with volatile byte reads. `false`
/// (nothing copied) if the range is not entirely inside the physmap.
///
/// Reads only, and bounds-checked: a stale value from a racing writer is a stale value, not a fault, so
/// this is safe for display and for firmware tables read once at boot.
#[must_use]
pub fn peek_ram(pa: PhysAddr, out: &mut [u8]) -> bool {
    let Some(end) = pa.as_usize().checked_add(out.len()) else {
        return false;
    };
    let inside = |a: usize| akuma_primitives::addr::physmap_contains(a);
    if !out.is_empty() && (!inside(pa.as_usize()) || !inside(end - 1)) {
        return false;
    }
    for (i, slot) in out.iter_mut().enumerate() {
        // SAFETY: both ends of the range are inside the physmap (checked above), which maps RAM
        // for the kernel's lifetime; a volatile `u8` read has no alignment or validity requirement.
        *slot = unsafe { phys_ptr::<u8>(pa + i as u64).read_volatile() };
    }
    true
}

/// `akuma-pagetable`'s view of the physmap: a table frame's kernel-virtual address.
pub struct Physmap;

impl akuma_pagetable::PhysMap for Physmap {
    fn table_addr(pa: u64) -> usize {
        phys_ptr::<u8>(PhysAddr::new(pa)) as usize
    }
}

/// A frame reached through the physmap without owning it: a page-table frame, or
/// a user page the kernel fills and drains. The shared-frame counterpart of
/// [`FreshFrame`].
///
/// Every access is a bounds-checked **volatile or raw-pointer copy** — no `&` or
/// `&mut` to the frame's bytes is ever formed, so two `Frame`s naming the same
/// page (a CoW share, a table being edited while it is read) cannot violate
/// Rust's aliasing rules, and the methods are safe. The one obligation sits on
/// [`Frame::from_pa`], once, where an address enters: it names RAM that no Rust
/// object lives in. [`Frame::alloc_zeroed`] needs none — a frame fresh from the
/// PMM is nobody's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Frame(PhysAddr);

impl Frame {
    /// Name the frame at `pa`.
    ///
    /// # Panics
    /// If `pa` is not page-aligned or is outside the physmap.
    ///
    /// # Safety
    /// `pa` is a frame of RAM that no Rust object (heap allocation, `static`,
    /// stack) lives in: a page-table frame or a user page the PMM handed out. The
    /// callee reads it as plain data and, through the `set_*`/`write_*`/`copy_*`
    /// methods, rewrites it as plain data.
    #[must_use]
    pub unsafe fn from_pa(pa: PhysAddr) -> Self {
        assert!(pa.is_page_aligned(), "Frame::from_pa: unaligned");
        assert!(akuma_primitives::addr::physmap_contains(pa.as_usize()), "Frame::from_pa: outside the physmap");
        Self(pa)
    }

    /// A zeroed frame from the PMM, owned by the caller until it maps or frees it.
    #[must_use]
    pub fn alloc_zeroed() -> Option<Self> {
        akuma_pmm::alloc_page_zeroed().map(|pa| Self(PhysAddr::from_usize(pa)))
    }

    /// The physical address.
    #[must_use]
    pub fn pa(self) -> PhysAddr {
        self.0
    }

    fn entry_ptr(self, i: usize) -> *mut u64 {
        assert!(i < PAGE_SIZE / 8, "Frame: table index out of range");
        phys_ptr::<u64>(self.0).wrapping_add(i)
    }

    /// Entry `i` of this frame read as a 512-entry page table.
    ///
    /// # Panics
    /// If `i >= 512`.
    #[must_use]
    pub fn entry(self, i: usize) -> u64 {
        // SAFETY: `entry_ptr` is inside this frame (index asserted), 8-aligned, and
        // `from_pa`'s contract makes the frame plain RAM; volatile, so no reference
        // to it exists.
        unsafe { self.entry_ptr(i).read_volatile() }
    }

    /// Overwrite entry `i`.
    ///
    /// # Panics
    /// If `i >= 512`.
    pub fn set_entry(self, i: usize, v: u64) {
        // SAFETY: as `entry`.
        unsafe { self.entry_ptr(i).write_volatile(v) }
    }

    /// Copy `dst.len()` bytes out of the frame starting at byte `off`.
    ///
    /// # Panics
    /// If the range leaves the frame.
    pub fn read_bytes(self, off: usize, dst: &mut [u8]) {
        assert!(off <= PAGE_SIZE && dst.len() <= PAGE_SIZE - off, "Frame::read_bytes out of range");
        // SAFETY: the source range is inside this frame (asserted) and plain RAM
        // (`from_pa`); `dst` is a Rust slice outside the physmap's frames, so the
        // two cannot overlap.
        unsafe { core::ptr::copy_nonoverlapping(phys_ptr::<u8>(self.0 + off as u64), dst.as_mut_ptr(), dst.len()) }
    }

    /// Copy `src` into the frame starting at byte `off`.
    ///
    /// # Panics
    /// If the range leaves the frame.
    pub fn write_bytes(self, off: usize, src: &[u8]) {
        assert!(off <= PAGE_SIZE && src.len() <= PAGE_SIZE - off, "Frame::write_bytes out of range");
        // SAFETY: as `read_bytes`, mirrored.
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), phys_ptr::<u8>(self.0 + off as u64), src.len()) }
    }

    /// Overwrite this whole frame with `src`'s.
    ///
    /// # Panics
    /// If `src` is this frame.
    pub fn copy_page_from(self, src: Self) {
        assert_ne!(self.0, src.0, "Frame::copy_page_from: a frame cannot be its own source");
        // SAFETY: two distinct whole frames of plain RAM (`from_pa`), so the ranges
        // are disjoint.
        unsafe { core::ptr::copy_nonoverlapping(phys_ptr::<u8>(src.0), phys_ptr::<u8>(self.0), PAGE_SIZE) }
    }
}

/// A page frame this code owns **exclusively** and has not yet published to any
/// page table, ledger or other owner.
///
/// That is the one situation where a `&mut` view of a frame through the physmap is
/// sound, so this type is where the physmap's `unsafe` lives for "stage a page
/// before it is mapped": a demand-zero fault, a CoW copy's destination, a loaded
/// segment, a test program's code and stack. A `fn zero_page(pa: u64)` would be
/// unsound — any caller could pass a frame somebody else is using.
///
/// Dropping it returns the frame to the PMM, so an early return between
/// allocation and mapping no longer leaks. **Hand the frame on with
/// [`FreshFrame::publish`] the moment it is mapped or tracked**: dropping a frame
/// that a page table already names frees it out from under that mapping.
pub struct FreshFrame {
    pa: PhysAddr,
}

impl FreshFrame {
    /// Allocate a frame with whatever its previous owner left in it. Use
    /// [`FreshFrame::alloc_zeroed`] unless every byte is about to be overwritten.
    #[must_use]
    pub fn alloc() -> Option<Self> {
        akuma_pmm::alloc_page().map(|pa| Self { pa: PhysAddr::from_usize(pa) })
    }

    /// Allocate a zeroed frame. Zero *before* mapping: a recycled frame otherwise
    /// leaks the previous owner's bytes to whoever reads it next.
    #[must_use]
    pub fn alloc_zeroed() -> Option<Self> {
        let mut frame = Self::alloc()?;
        frame.bytes_mut().fill(0);
        Some(frame)
    }

    /// Allocate a zeroed frame through the PMM's own `alloc_page_zeroed`, which
    /// zeroes **and cleans the data cache** to the point of coherency. That is
    /// what an AArch64 user page needs before it is mapped; [`FreshFrame::alloc_zeroed`]
    /// zeroes through the physmap only. Same ownership as every `FreshFrame`.
    #[must_use]
    pub fn alloc_zeroed_clean() -> Option<Self> {
        akuma_pmm::alloc_page_zeroed().map(|pa| Self { pa: PhysAddr::from_usize(pa) })
    }

    /// As [`FreshFrame::alloc_zeroed_clean`], for a **user** demand-paging fault:
    /// `None` once free PMM has fallen to the user reserve (the caller treats that
    /// as OOM), and the reclaim escalation runs first.
    #[must_use]
    pub fn alloc_zeroed_user() -> Option<Self> {
        akuma_pmm::alloc_page_zeroed_user().map(|pa| Self { pa: PhysAddr::from_usize(pa) })
    }

    /// Allocate `count` zeroed (and cache-cleaned) frames in one PMM lock hold, none
    /// of them contiguous by promise. `None` allocates nothing: no partial batch.
    /// Each is its own `FreshFrame`, so a frame the caller never gets to map is
    /// returned to the PMM when its slot is dropped.
    #[must_use]
    pub fn alloc_batch_zeroed(count: usize) -> Option<alloc::vec::Vec<Self>> {
        let pas = akuma_pmm::alloc_pages_zeroed(count)?;
        Some(pas.into_iter().map(|pa| Self { pa: PhysAddr::from_usize(pa) }).collect())
    }

    /// The frame's physical address, for mapping it. Does not give it up.
    #[must_use]
    pub fn pa(&self) -> PhysAddr {
        self.pa
    }

    /// Run `f` over `len` bytes of the frame starting at `off` — a file read or a
    /// code copy that lands straight in the staged page.
    ///
    /// # Panics
    /// If `off + len` runs past the end of the page.
    pub fn fill_with<R>(&mut self, off: usize, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
        let end = off.checked_add(len).expect("fill_with: range overflows");
        assert!(end <= PAGE_SIZE, "fill_with: range past the end of the page");
        f(&mut self.bytes_mut()[off..end])
    }

    /// The frame's bytes.
    pub fn bytes_mut(&mut self) -> &mut [u8; PAGE_SIZE] {
        // SAFETY: a PMM frame this value owns alone (nothing maps or tracks it
        // yet, and `publish` consumes the only handle), reached through the
        // physmap, which covers every PMM frame; `[u8; N]` has no alignment or
        // validity requirement, and `&mut self` makes this the only live view.
        unsafe { &mut *phys_ptr::<[u8; PAGE_SIZE]>(self.pa) }
    }

    /// Overwrite this frame with the contents of the live frame at `src_pa`.
    ///
    /// `src_pa` must be a frame nothing is writing while this runs — the CoW
    /// source, which every mapper holds read-only. The copy is one page of
    /// physmap-reachable RAM; nothing here can fault or alias `self`, but a
    /// concurrent writer to `src_pa` would tear the copy.
    ///
    /// # Panics
    /// If `src_pa` is not page-aligned or is outside the physmap.
    pub fn copy_from_frame(&mut self, src_pa: PhysAddr) {
        assert!(src_pa.is_page_aligned(), "copy_from_frame: unaligned source");
        assert_ne!(src_pa, self.pa, "copy_from_frame: a frame cannot be its own source");
        // SAFETY: `phys_ptr` asserts `src_pa` is inside the physmap; the frame is
        // a whole page of RAM; it is not `self.pa` (checked), so the shared view
        // cannot alias the `&mut` that `bytes_mut` hands out; and the caller's
        // contract above rules out a concurrent writer.
        let src = unsafe { &*phys_ptr::<[u8; PAGE_SIZE]>(src_pa) };
        self.bytes_mut().copy_from_slice(src);
    }

    /// Give the frame up: it is now mapped or tracked by whoever called this, and
    /// is theirs to free. Returns its physical address.
    #[must_use = "the frame is now the caller's to map or free; dropping the address leaks it"]
    pub fn publish(self) -> usize {
        let pa = self.pa.as_usize();
        core::mem::forget(self);
        pa
    }
}

impl FreshFrame {
    /// Return the frame to the PMM now, attributing the release to thread `tid`
    /// (what a premature-free report names). `Drop` does the same with `tid` 0.
    pub fn free_by(self, tid: u32) {
        self.free_at(tid, akuma_pmm::FreeSite::Unknown);
    }

    /// As [`FreshFrame::free_by`], naming the code path for premature-free reports.
    pub fn free_at(self, tid: u32, site: akuma_pmm::FreeSite) {
        let pa = self.pa.as_usize();
        core::mem::forget(self);
        akuma_pmm::free_page_at(pa, tid, site);
    }
}

impl Drop for FreshFrame {
    fn drop(&mut self) {
        akuma_pmm::free_page(self.pa.as_usize(), 0);
    }
}

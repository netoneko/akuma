//! What the arch-neutral half asks of a user page table.
//!
//! The machine half implements [`UserPages`] over its own walker and entry
//! format: the x86 4-level table on amd64, the AArch64 translation table on
//! aarch64. Every method is an associated function (no `self`) because there is
//! exactly one machine in a kernel, so a call is a direct call. Entries cross the
//! seam as raw `u64` only where they are opaque here: core reads one through
//! [`lazy_record::decode`](crate::lazy_record::decode), which is the same layout
//! on both architectures, and otherwise only hands it back.

use crate::lazy_record::Lazy;

/// A page-sized frame nothing maps or tracks yet: the machine's physmap view of a
/// fresh PMM frame. Dropping it returns the frame to the PMM.
pub trait FreshPage: Sized {
    /// A frame with whatever its previous owner left in it.
    fn alloc() -> Option<Self>;
    /// A zeroed frame.
    fn alloc_zeroed() -> Option<Self>;
    /// Its physical address, for mapping it. Does not give it up.
    fn pa(&self) -> usize;
    /// Its bytes.
    fn bytes_mut(&mut self) -> &mut [u8; 4096];
    /// Overwrite it with the live frame at `src_pa`, which nothing writes while
    /// this runs (a copy-on-write source every mapper holds read-only).
    fn copy_from(&mut self, src_pa: usize);
    /// Give it up: it is now mapped or tracked by the caller. Returns its address.
    fn publish(self) -> usize;
}

/// A user address space's page table, named by its root's physical address.
pub trait UserPages: 'static {
    /// The machine's fresh frame.
    type Frame: FreshPage;

    /// The 4 KiB leaf entry for `va`, when every table above it exists (a huge
    /// page counts as absent).
    fn leaf(root: u64, va: u64) -> Option<u64>;
    /// Overwrite the leaf for `va` in a table that already exists. `false` when
    /// it does not. No TLB maintenance: the caller writes only invalid entries
    /// here, or invalidates after.
    fn write_leaf(root: u64, va: u64, entry: u64) -> bool;
    /// Store `entry` as the leaf for `va`, allocating tables on the way.
    /// `false` when a table frame cannot be had.
    fn set_leaf(root: u64, va: u64, entry: u64) -> bool;
    /// Map `pa` at user `va`, write-back memory, with these permissions.
    fn map_user(root: u64, va: u64, pa: u64, write: bool, exec: bool) -> bool;
    /// Drop this core's translation for `va` in the current address space.
    fn invalidate(va: u64);
    /// May user code write through the valid entry `pte`?
    fn writable(pte: u64) -> bool;
    /// Is `pte` marked as a copy-on-write share (read-only because a fork shared
    /// it, not because the mapping is)?
    fn cow_marked(pte: u64) -> bool;
    /// Has the page been written since it was mapped (the W^X record)?
    fn written(pte: u64) -> bool;
    /// `pte`, recorded as written.
    fn with_written(pte: u64) -> u64;
    /// `pte`, writable and unmarked: this space takes the frame in place.
    fn take(pte: u64) -> u64;
    /// `pte`'s attributes on frame `pa`, writable and unmarked: the private copy.
    fn take_at(pte: u64, pa: u64) -> u64;
    /// `pte`, read-only and marked: a share the next write must break.
    fn share_marked(pte: u64) -> u64;
    /// The lazy record a valid leaf `pte` would become if dropped and refaulted
    /// as zeros: same permissions, no file (`MADV_DONTNEED`). Reading
    /// permissions from a valid entry is the machine's encoding.
    fn anon_like(pte: u64) -> Lazy;
}

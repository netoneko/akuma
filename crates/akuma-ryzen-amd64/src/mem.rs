//! Reading physical memory, abstracted so the parsers are host-testable.
//!
//! The aarch64 sibling `akuma-firecracker` takes a `&[u8]` — a device tree is
//! one contiguous blob, so a slice is the whole machine description. This
//! machine's description is **scattered**: the handoff block is at one physical
//! address, its memory map at another, the command line at a third, and the ACPI
//! tables wherever the VMM put them. A slice cannot express that, and copying
//! everything into one before parsing would need an allocator that does not
//! exist yet at the point this runs.
//!
//! So the parsers take a reader instead. In the kernel it is three lines over
//! the physmap; in a test it is a sparse map of `(address, bytes)` pairs, which
//! is what lets every parser below be exercised against **measured bytes from a
//! real machine** on the development host.

/// Somewhere physical memory can be read from, with bounds enforced by the
/// implementor.
///
/// Returning `false` must mean "I will not read that", never a partial fill: a
/// parser that saw half a structure would carry on with the other half
/// uninitialised. Every implementation therefore either fills `buf` completely
/// or leaves it alone.
pub trait PhysMem {
    /// Fill `buf` from physical address `pa`, or refuse.
    fn read(&self, pa: u64, buf: &mut [u8]) -> bool;
}

/// Read a fixed-size array, or `None`.
pub(crate) fn read_n<const N: usize, M: PhysMem + ?Sized>(m: &M, pa: u64) -> Option<[u8; N]> {
    let mut buf = [0u8; N];
    m.read(pa, &mut buf).then_some(buf)
}

pub(crate) fn read_u16<M: PhysMem + ?Sized>(m: &M, pa: u64) -> Option<u16> {
    read_n::<2, M>(m, pa).map(u16::from_le_bytes)
}

pub(crate) fn read_u32<M: PhysMem + ?Sized>(m: &M, pa: u64) -> Option<u32> {
    read_n::<4, M>(m, pa).map(u32::from_le_bytes)
}

pub(crate) fn read_u64<M: PhysMem + ?Sized>(m: &M, pa: u64) -> Option<u64> {
    read_n::<8, M>(m, pa).map(u64::from_le_bytes)
}

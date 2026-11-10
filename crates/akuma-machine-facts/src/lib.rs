//! **What the machine told us**, in one shape for both architectures.
//!
//! The AArch64 kernel learns its machine from an FDT (`akuma-firecracker::describe`) or, before
//! the FDT is readable, from compile-time constants (`akuma-kernel-core::platform`). The amd64
//! kernel learns it from a PVH `hvm_start_info` plus ACPI/MADT (`akuma-ryzen-amd64`) or from
//! multiboot2. Each description is shaped by its own protocol; this crate is the part every kernel
//! decision actually needs, with no protocol in it
//! (`docs/archive/AKUMA_ARCH_CONVERGENCE_RESEARCH.md` §5, slice M2).
//!
//! Deliberately **small**: only what both discovery paths supply today. Console, timer, RNG and
//! the reboot conduit are arch constants rather than discovered facts, so they are not here; when
//! one becomes discovered, it joins this struct with an adapter that fills it and a test.
//!
//! Pure, `forbid(unsafe_code)`, no dependencies, no allocation: fixed arrays, `Copy` values.
//! The per-protocol crates depend on this one (never the reverse) and own the adapters, so a
//! change to a protocol parser cannot silently change what the rest of the kernel believes.

#![no_std]
#![forbid(unsafe_code)]

/// RAM regions kept. The amd64 memory map's own cap (`akuma-ryzen-amd64::MAX_REGIONS`).
pub const MAX_RAM: usize = 16;
/// virtio-MMIO transports kept. Firecracker and QEMU `microvm` both stay under this.
pub const MAX_VIRTIO: usize = 8;

/// A span of physical address space. `end` saturates: a VMM-supplied `base + size` that
/// overflows must not become a region that appears to start after it ends.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Range {
    pub base: u64,
    pub size: u64,
}

impl Range {
    #[must_use]
    pub const fn new(base: u64, size: u64) -> Self {
        Self { base, size }
    }

    #[must_use]
    pub const fn end(self) -> u64 {
        self.base.saturating_add(self.size)
    }

    #[must_use]
    pub const fn contains(self, addr: u64) -> bool {
        addr >= self.base && addr < self.end()
    }

    /// Does `self` wholly contain `other`? An empty `other` is covered by anything that contains
    /// its base.
    #[must_use]
    pub const fn covers(self, other: Self) -> bool {
        self.base <= other.base && other.end() <= self.end()
    }

    /// Do the two spans share any byte? Empty spans overlap nothing.
    #[must_use]
    pub const fn overlaps(self, other: Self) -> bool {
        self.size != 0 && other.size != 0 && self.base < other.end() && other.base < self.end()
    }
}

/// One virtio-MMIO transport: its register window and its interrupt line (a GIC INTID on
/// AArch64, a GSI routed through the IOAPIC on amd64 — the number means what the controller says).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct VirtioMmio {
    pub window: Range,
    pub irq: u32,
}

/// The interrupt controller, as far as discovery can tell.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IrqController {
    /// Discovery did not say (compile-time facts before the FDT is read, or no MADT).
    Unknown,
    Gicv2 { distributor: Range },
    Gicv3 { distributor: Range, redistributors: Option<Range> },
    /// Local APIC + the first IOAPIC's MMIO base.
    IoApic { base: u64 },
}

/// Where the facts came from. Printed with them: "which protocol told us this" is the first
/// question when a machine misbehaves, and the answer used to be inferred from the log.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Provenance {
    /// Compile-time constants for a known machine (`qemu-virt`, `firecracker`).
    CompileTime,
    /// A flattened device tree.
    Fdt,
    /// A PVH `hvm_start_info` (+ ACPI).
    Pvh,
    /// A multiboot2 information block (+ ACPI).
    Multiboot2,
}

impl Provenance {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::CompileTime => "compile-time",
            Self::Fdt => "fdt",
            Self::Pvh => "pvh",
            Self::Multiboot2 => "multiboot2",
        }
    }
}

/// Why a [`Facts`] is not usable.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FactsError {
    /// No RAM region, or all of them empty: nothing to allocate from.
    NoRam,
    /// Two RAM regions share a byte. A frame allocator would hand the same frame out twice.
    RamOverlap,
    /// A virtio window sits inside RAM. Either the map or the device list is wrong.
    VirtioInRam,
    /// Two virtio windows share a byte.
    VirtioOverlap,
    /// A CPU count of zero: the boot processor is a CPU.
    ZeroCpus,
}

/// The neutral machine description. Build with [`Facts::new`] and the `push_*` methods; check with
/// [`Facts::validate`].
#[derive(Copy, Clone, Debug)]
pub struct Facts {
    pub provenance: Provenance,
    ram: [Range; MAX_RAM],
    ram_count: usize,
    virtio: [VirtioMmio; MAX_VIRTIO],
    virtio_count: usize,
    /// CPUs the machine lists (MADT entries, FDT `cpu` nodes); `None` when discovery has no count.
    pub cpus: Option<usize>,
    pub irq: IrqController,
}

impl Facts {
    #[must_use]
    pub const fn new(provenance: Provenance) -> Self {
        Self {
            provenance,
            ram: [Range::new(0, 0); MAX_RAM],
            ram_count: 0,
            virtio: [VirtioMmio { window: Range::new(0, 0), irq: 0 }; MAX_VIRTIO],
            virtio_count: 0,
            cpus: None,
            irq: IrqController::Unknown,
        }
    }

    /// Add a RAM region; `false` (and no change) when the table is full. Empty regions are
    /// dropped: they carry no frame.
    pub fn push_ram(&mut self, r: Range) -> bool {
        if r.size == 0 {
            return true;
        }
        if self.ram_count == MAX_RAM {
            return false;
        }
        self.ram[self.ram_count] = r;
        self.ram_count += 1;
        true
    }

    /// Add a virtio-MMIO transport; `false` when the table is full.
    pub fn push_virtio(&mut self, d: VirtioMmio) -> bool {
        if self.virtio_count == MAX_VIRTIO {
            return false;
        }
        self.virtio[self.virtio_count] = d;
        self.virtio_count += 1;
        true
    }

    #[must_use]
    pub fn ram(&self) -> &[Range] {
        &self.ram[..self.ram_count]
    }

    #[must_use]
    pub fn virtio(&self) -> &[VirtioMmio] {
        &self.virtio[..self.virtio_count]
    }

    /// Total RAM bytes, saturating.
    #[must_use]
    pub fn ram_bytes(&self) -> u64 {
        self.ram().iter().fold(0u64, |a, r| a.saturating_add(r.size))
    }

    /// The RAM region holding `pa`, if any.
    #[must_use]
    pub fn ram_containing(&self, pa: u64) -> Option<Range> {
        self.ram().iter().copied().find(|r| r.contains(pa))
    }

    /// Is `span` wholly inside one RAM region and clear of every `keep_out` range?
    ///
    /// The boot loader may have placed things in RAM that the memory map does not mention (a PVH
    /// start-info block, GRUB's information block, a root-image module). A caller about to write
    /// to a fixed page — the AP trampoline is the one today — names what it must not clobber and
    /// asks. `false` is a configuration ("stay single-core"), not a failure.
    #[must_use]
    pub fn is_clear_ram(&self, span: Range, keep_out: impl IntoIterator<Item = Range>) -> bool {
        self.ram().iter().any(|r| r.covers(span)) && keep_out.into_iter().all(|k| !k.overlaps(span))
    }

    /// Check the invariants every consumer assumes. `O(n²)` over at most `MAX_RAM`/`MAX_VIRTIO`
    /// entries, once, at boot.
    ///
    /// # Errors
    /// The first violated invariant, in the order of [`FactsError`].
    pub fn validate(&self) -> Result<(), FactsError> {
        if self.ram().is_empty() {
            return Err(FactsError::NoRam);
        }
        for (i, a) in self.ram().iter().enumerate() {
            if self.ram()[i + 1..].iter().any(|b| a.overlaps(*b)) {
                return Err(FactsError::RamOverlap);
            }
        }
        for (i, d) in self.virtio().iter().enumerate() {
            if self.ram().iter().any(|r| r.overlaps(d.window)) {
                return Err(FactsError::VirtioInRam);
            }
            if self.virtio()[i + 1..].iter().any(|o| d.window.overlaps(o.window)) {
                return Err(FactsError::VirtioOverlap);
            }
        }
        if self.cpus == Some(0) {
            return Err(FactsError::ZeroCpus);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok() -> Facts {
        let mut f = Facts::new(Provenance::Pvh);
        assert!(f.push_ram(Range::new(0x10_0000, 0x7ff0_0000)));
        assert!(f.push_virtio(VirtioMmio { window: Range::new(0xfeb0_0000, 0x200), irq: 5 }));
        assert!(f.push_virtio(VirtioMmio { window: Range::new(0xfeb0_0200, 0x200), irq: 6 }));
        f.cpus = Some(4);
        f
    }

    #[test]
    fn a_sane_machine_validates() {
        assert_eq!(ok().validate(), Ok(()));
    }

    #[test]
    fn no_ram_is_refused() {
        assert_eq!(Facts::new(Provenance::Fdt).validate(), Err(FactsError::NoRam));
    }

    #[test]
    fn empty_regions_are_dropped_not_counted() {
        let mut f = Facts::new(Provenance::Fdt);
        assert!(f.push_ram(Range::new(0x4000_0000, 0)));
        assert!(f.ram().is_empty());
        assert_eq!(f.validate(), Err(FactsError::NoRam));
    }

    #[test]
    fn overlapping_ram_is_refused() {
        let mut f = ok();
        assert!(f.push_ram(Range::new(0x1000_0000, 0x1000)));
        assert_eq!(f.validate(), Err(FactsError::RamOverlap));
    }

    #[test]
    fn adjacent_ram_is_not_an_overlap() {
        let mut f = Facts::new(Provenance::Pvh);
        assert!(f.push_ram(Range::new(0, 0x1000)));
        assert!(f.push_ram(Range::new(0x1000, 0x1000)));
        assert_eq!(f.validate(), Ok(()));
    }

    #[test]
    fn a_virtio_window_inside_ram_is_refused() {
        let mut f = ok();
        assert!(f.push_virtio(VirtioMmio { window: Range::new(0x20_0000, 0x200), irq: 7 }));
        assert_eq!(f.validate(), Err(FactsError::VirtioInRam));
    }

    #[test]
    fn overlapping_virtio_windows_are_refused() {
        let mut f = ok();
        assert!(f.push_virtio(VirtioMmio { window: Range::new(0xfeb0_0100, 0x200), irq: 7 }));
        assert_eq!(f.validate(), Err(FactsError::VirtioOverlap));
    }

    #[test]
    fn zero_cpus_is_refused_but_unknown_is_fine() {
        let mut f = ok();
        f.cpus = Some(0);
        assert_eq!(f.validate(), Err(FactsError::ZeroCpus));
        f.cpus = None;
        assert_eq!(f.validate(), Ok(()));
    }

    #[test]
    fn tables_report_full_instead_of_overflowing() {
        let mut f = Facts::new(Provenance::Pvh);
        for i in 0..MAX_RAM as u64 {
            assert!(f.push_ram(Range::new(i * 0x1000, 0x1000)));
        }
        assert!(!f.push_ram(Range::new(0x100_0000, 0x1000)));
        for i in 0..MAX_VIRTIO as u64 {
            assert!(f.push_virtio(VirtioMmio { window: Range::new(0xfeb0_0000 + i * 0x200, 0x200), irq: 0 }));
        }
        assert!(!f.push_virtio(VirtioMmio::default()));
    }

    #[test]
    fn range_end_saturates_and_overlap_ignores_empty() {
        let r = Range::new(u64::MAX - 4, 100);
        assert_eq!(r.end(), u64::MAX);
        assert!(!r.overlaps(Range::new(0, 0)));
        assert!(!Range::new(10, 0).overlaps(Range::new(0, 100)));
        assert!(Range::new(10, 5).overlaps(Range::new(12, 5)));
    }

    #[test]
    fn ram_containing_finds_the_region() {
        let f = ok();
        assert_eq!(f.ram_containing(0x20_0000).map(|r| r.base), Some(0x10_0000));
        assert_eq!(f.ram_containing(0x50), None);
        assert_eq!(f.ram_bytes(), 0x7ff0_0000);
    }

    #[test]
    fn a_span_inside_ram_and_clear_of_keep_out_is_clear_ram() {
        let f = ok();
        let page = Range::new(0x20_8000, 0x1000);
        assert!(f.is_clear_ram(page, []));
        assert!(f.is_clear_ram(page, [Range::new(0x20_9000, 0x1000), Range::new(0x20_7000, 0x1000)]));
    }

    #[test]
    fn keep_out_touching_the_span_refuses_it() {
        let f = ok();
        let page = Range::new(0x10_8000, 0x1000);
        assert!(!f.is_clear_ram(page, [Range::new(0x10_8fff, 1)]));
        assert!(!f.is_clear_ram(page, [Range::new(0x10_0000, 0x10_0000)]));
        // an empty keep-out (the "no module" sentinel `(0, 0)`) overlaps nothing
        assert!(f.is_clear_ram(page, [Range::new(0, 0)]));
    }

    #[test]
    fn a_span_outside_or_straddling_ram_is_not_clear_ram() {
        let f = ok(); // RAM starts at 0x10_0000
        assert!(!f.is_clear_ram(Range::new(0x8000, 0x1000), []), "below RAM");
        assert!(!f.is_clear_ram(Range::new(0x10_0000 - 0x800, 0x1000), []), "straddles the start");
        let end = 0x10_0000 + 0x7ff0_0000;
        assert!(f.is_clear_ram(Range::new(end - 0x1000, 0x1000), []), "last page");
        assert!(!f.is_clear_ram(Range::new(end - 0x800, 0x1000), []), "straddles the end");
    }

    #[test]
    fn a_span_split_across_two_ram_regions_is_not_clear_ram() {
        let mut f = Facts::new(Provenance::Pvh);
        assert!(f.push_ram(Range::new(0, 0x1000)));
        assert!(f.push_ram(Range::new(0x1000, 0x1000)));
        assert!(!f.is_clear_ram(Range::new(0x800, 0x1000), []), "adjacent regions are two regions");
    }

    #[test]
    fn range_covers_is_containment() {
        assert!(Range::new(0, 10).covers(Range::new(2, 3)));
        assert!(Range::new(0, 10).covers(Range::new(0, 10)));
        assert!(!Range::new(0, 10).covers(Range::new(5, 6)));
        assert!(!Range::new(5, 10).covers(Range::new(4, 2)));
    }

    #[test]
    fn provenance_names_are_stable() {
        assert_eq!(Provenance::CompileTime.name(), "compile-time");
        assert_eq!(Provenance::Fdt.name(), "fdt");
        assert_eq!(Provenance::Pvh.name(), "pvh");
        assert_eq!(Provenance::Multiboot2.name(), "multiboot2");
    }
}

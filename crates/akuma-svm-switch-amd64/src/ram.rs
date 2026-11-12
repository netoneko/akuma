//! Guest RAM as an owned value.
//!
//! A [`GuestRam`] is a physically contiguous run of host frames that belongs to exactly one
//! guest: the only way to make one is to take it from the page allocator ([`GuestRam::alloc`])
//! or from the `kvmmem=` reserve ([`reserve::carve`]), and dropping it gives it back to the
//! one it came from. [`crate::Npt`] maps nothing else, so a nested page table cannot point a
//! guest at memory the host still uses — the property `VMRUN`'s safety rests on.
//!
//! The host reads and writes guest RAM through its physical address (`pa`) with the physmap
//! copies; the value only has to outlive those uses, which holding it (or the [`crate::Npt`]
//! that adopted it) guarantees.

use akuma_vm_spec::arena::Arena;
use spinning_top::Spinlock;

/// Where the frames go back to.
#[derive(Debug)]
enum Src {
    /// `akuma_pmm::alloc_pages_contiguous_zeroed`'s block (the aligned start is inside it).
    Pmm { alloc_pa: usize, pages: usize },
    /// A slice of the reserve; `off` is its arena handle.
    Reserve { off: u64 },
}

/// Zeroed, physically contiguous guest RAM: `bytes` from host-physical `pa`.
#[derive(Debug)]
pub struct GuestRam {
    pa: u64,
    bytes: u64,
    src: Src,
}

impl GuestRam {
    /// `bytes` (a 4 KiB multiple) from the page allocator, starting `align`-aligned (a power of
    /// two, at least 4 KiB): the block is over-allocated by `align - 4 KiB` and the whole of
    /// it is freed on drop.
    #[must_use]
    pub fn alloc(bytes: u64, align: u64) -> Option<Self> {
        if bytes == 0 || !bytes.is_multiple_of(4096) || !align.is_power_of_two() || align < 4096 {
            return None;
        }
        let pages = usize::try_from(bytes.checked_add(align - 4096)? / 4096).ok()?;
        let alloc_pa = akuma_pmm::alloc_pages_contiguous_zeroed(pages)?;
        let pa = (alloc_pa as u64 + align - 1) & !(align - 1);
        Some(Self { pa, bytes, src: Src::Pmm { alloc_pa, pages } })
    }

    /// Host-physical start.
    #[must_use]
    pub const fn pa(&self) -> u64 {
        self.pa
    }

    /// Length in bytes.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Did it come from the reserve?
    #[must_use]
    pub const fn is_reserved(&self) -> bool {
        matches!(self.src, Src::Reserve { .. })
    }
}

impl Drop for GuestRam {
    fn drop(&mut self) {
        match self.src {
            Src::Pmm { alloc_pa, pages } => akuma_pmm::free_pages_contiguous(alloc_pa, pages),
            Src::Reserve { off } => reserve::give_back(off),
        }
    }
}

/// The `kvmmem=` reserve: one contiguous block taken from the page allocator at boot, while
/// memory is unfragmented, and never returned; guests' RAM is carved out of it.
pub mod reserve {
    use super::{Arena, GuestRam, Spinlock, Src};

    struct State {
        /// Host-physical, `align`-aligned.
        base: u64,
        align: u64,
        arena: Arena,
    }

    static RESERVE: Spinlock<Option<State>> = Spinlock::new(None);

    fn locked<T>(f: impl FnOnce(&mut Option<State>) -> T) -> T {
        akuma_primitives::irq::with_irqs_disabled(|| f(&mut RESERVE.lock()))
    }

    /// What [`init`] did.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Init {
        Reserved,
        /// There already is one: a second would orphan every slice carved from the first.
        AlreadyReserved,
        /// No contiguous block that large.
        NoMemory,
    }

    /// Take `bytes` of contiguous memory for guests, slices starting `align`-aligned.
    #[must_use]
    pub fn init(bytes: u64, align: u64) -> Init {
        if locked(|r| r.is_some()) {
            return Init::AlreadyReserved;
        }
        if bytes == 0 || !align.is_power_of_two() || align < 4096 {
            return Init::NoMemory;
        }
        let Some(pages) = bytes.checked_add(align).and_then(|b| usize::try_from(b / 4096).ok()) else {
            return Init::NoMemory;
        };
        let Some(alloc_pa) = akuma_pmm::alloc_pages_contiguous_zeroed(pages) else { return Init::NoMemory };
        let base = (alloc_pa as u64 + align - 1) & !(align - 1);
        let placed = locked(|r| {
            if r.is_some() {
                return false;
            }
            *r = Some(State { base, align, arena: Arena::new(bytes) });
            true
        });
        if placed {
            Init::Reserved
        } else {
            akuma_pmm::free_pages_contiguous(alloc_pa, pages);
            Init::AlreadyReserved
        }
    }

    /// Where a slice came from.
    #[derive(Debug)]
    pub enum Carve {
        /// A zeroed slice of the reserve.
        Ram(GuestRam),
        /// No reserve was configured: the caller may use [`GuestRam::alloc`].
        NoReserve,
        /// A reserve exists and has no room: **refuse** (no silent fallback).
        Refused { free: u64 },
    }

    /// Slice `bytes` (a 4 KiB multiple) out of the reserve, zeroed.
    #[must_use]
    pub fn carve(bytes: u64) -> Carve {
        if bytes == 0 || !bytes.is_multiple_of(4096) {
            return Carve::Refused { free: usage().map_or(0, |(u, t)| t - u) };
        }
        let got = locked(|r| {
            let s = r.as_mut()?;
            Some(match s.arena.alloc(bytes, s.align) {
                Some(off) => Ok((s.base + off, off)),
                None => Err(s.arena.total() - s.arena.used()),
            })
        });
        match got {
            None => Carve::NoReserve,
            Some(Err(free)) => Carve::Refused { free },
            Some(Ok((pa, off))) => {
                // The slice was another guest's: zero it before anyone sees it.
                let z = [0u8; 4096];
                let mut o = 0;
                while o < bytes {
                    let _ = akuma_mmu_raw::copy_to_phys((pa + o) as usize, &z);
                    o += 4096;
                }
                Carve::Ram(GuestRam { pa, bytes, src: Src::Reserve { off } })
            }
        }
    }

    pub(super) fn give_back(off: u64) {
        locked(|r| {
            if let Some(s) = r.as_mut() {
                s.arena.free(off);
            }
        });
    }

    /// Bytes in use and in total, for `status`.
    #[must_use]
    pub fn usage() -> Option<(u64, u64)> {
        locked(|r| r.as_ref().map(|s| (s.arena.used(), s.arena.total())))
    }
}

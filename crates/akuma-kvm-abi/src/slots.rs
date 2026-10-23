//! The guest-memory slot table.
//!
//! `SET_USER_MEMORY_REGION` is `{slot u32, flags u32, gpa u64, size u64, uaddr u64}`. The VMM's RAM is
//! its own anonymous memory (`uaddr`, 4 KiB aligned); this hypervisor instead gives the guest a
//! slice of the reserved block and **remaps the VMM's range onto it**, so a slot records both: where the
//! VMM sees the memory and which offset of the slice backs it. The nested page table then maps
//! `gpa -> slice base + slice offset` with 2 MiB leaves, which is why `gpa`, `size` and the slice
//! offset must be 2 MiB aligned and the guest bounded as `kvm0`'s RAM is (<= 1 GiB).

use crate::{rd32, rd64};

pub const ARG_LEN: usize = 32;
pub const MAX_SLOTS: usize = 8;
/// NPT leaf size.
pub const LEAF: u64 = 2 << 20;
/// The guest-physical ceiling the nested page table covers (one directory of 2 MiB leaves).
pub const GPA_LIMIT: u64 = 1 << 30;
/// The VMM's range must be page aligned.
pub const PAGE: u64 = 4096;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Region {
    pub slot: u32,
    pub flags: u32,
    pub gpa: u64,
    pub size: u64,
    pub uaddr: u64,
}

impl Region {
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        Some(Self { slot: rd32(b, 0)?, flags: rd32(b, 4)?, gpa: rd64(b, 8)?, size: rd64(b, 16)?, uaddr: rd64(b, 24)? })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// Slot number out of range.
    Slot,
    /// Any flag set (dirty logging, read-only): not supported.
    Flags,
    /// `gpa`/`size` not 2 MiB aligned, size zero, or beyond [`GPA_LIMIT`].
    Geometry,
    /// `uaddr` not page aligned, or the user range wraps / leaves the user half.
    UserRange,
    /// Overlaps a live slot in guest-physical space.
    Overlap,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Slot {
    pub region: Region,
    /// Offset in the reserved slice that backs the region's first byte.
    pub slice_off: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Table {
    slots: [Option<Slot>; MAX_SLOTS],
}

/// The top of the user half of the address space (canonical lower half).
pub const USER_TOP: u64 = 1 << 47;

impl Table {
    #[must_use]
    pub const fn new() -> Self {
        Self { slots: [None; MAX_SLOTS] }
    }

    /// Validate `r` against the geometry rules and the live slots; nothing is recorded.
    pub fn check(&self, r: &Region) -> Result<(), Refused> {
        if r.slot as usize >= MAX_SLOTS {
            return Err(Refused::Slot);
        }
        if r.flags != 0 {
            return Err(Refused::Flags);
        }
        let end = r.gpa.checked_add(r.size).ok_or(Refused::Geometry)?;
        if r.size == 0 || !r.gpa.is_multiple_of(LEAF) || !r.size.is_multiple_of(LEAF) || end > GPA_LIMIT {
            return Err(Refused::Geometry);
        }
        let uend = r.uaddr.checked_add(r.size).ok_or(Refused::UserRange)?;
        if !r.uaddr.is_multiple_of(PAGE) || uend > USER_TOP {
            return Err(Refused::UserRange);
        }
        for (i, s) in self.slots.iter().enumerate() {
            if let Some(s) = s
                && i != r.slot as usize
                && r.gpa < s.region.gpa + s.region.size
                && s.region.gpa < end
            {
                return Err(Refused::Overlap);
            }
        }
        Ok(())
    }

    /// Record a validated region backed at `slice_off` in the slice.
    pub fn set(&mut self, r: Region, slice_off: u64) -> Result<(), Refused> {
        self.check(&r)?;
        self.slots[r.slot as usize] = Some(Slot { region: r, slice_off });
        Ok(())
    }

    #[must_use]
    pub fn get(&self, slot: u32) -> Option<&Slot> {
        self.slots.get(slot as usize)?.as_ref()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Slot> {
        self.slots.iter().flatten()
    }

    /// Where guest-physical `gpa` lives: the offset in the slice.
    #[must_use]
    pub fn slice_offset(&self, gpa: u64) -> Option<u64> {
        self.iter()
            .find(|s| gpa >= s.region.gpa && gpa - s.region.gpa < s.region.size)
            .map(|s| s.slice_off + (gpa - s.region.gpa))
    }

    /// The VMM's virtual address of guest-physical `gpa` (for reading what the guest sees through the VMM's
    /// own mapping).
    #[must_use]
    pub fn user_addr(&self, gpa: u64) -> Option<u64> {
        self.iter()
            .find(|s| gpa >= s.region.gpa && gpa - s.region.gpa < s.region.size)
            .map(|s| s.region.uaddr + (gpa - s.region.gpa))
    }
}

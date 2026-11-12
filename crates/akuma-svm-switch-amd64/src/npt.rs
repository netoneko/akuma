//! The nested page table as an owned value (APM Vol 2 §15.25).
//!
//! An [`Npt`] owns every frame it consists of — its tables — and every frame it maps: the
//! [`GuestRam`]s it adopted and the single pages it adopted. A leaf can only be written by
//! [`Npt::map_ram`] (into adopted RAM, bounds-checked) or [`Npt::adopt_page`] (the page it
//! was just given), so whatever the guest reaches through it is the guest's alone. Mapping is
//! append-only: nothing is ever unmapped while the table lives, so no TLB entry can outlive
//! the memory it names before the whole table is dropped, and the table cannot be dropped
//! while a [`crate::Guest`] holds it (`Arc`).

use akuma_phys::FreshFrame;
use akuma_primitives::addr::PhysAddr;
use akuma_svm_amd64::npt::{leaf_2m, leaf_4k, pointer, ADDR_MASK, P, PS};
use akuma_svm_amd64::{rd64, wr64};

use crate::ram::GuestRam;

/// Intermediate tables (PDPTs, directories, page tables) one guest can need: four GiB of RAM in
/// 2 MiB leaves is a PDPT and five directories; the rehearsal guests' 4 KiB leaves add a few.
pub const MAX_TABLES: usize = 32;
/// Adopted RAM runs (the `/dev/kvm` API's memslots: `akuma_kvm_abi::slots::MAX_SLOTS`).
pub const MAX_RAM: usize = 8;
/// Adopted single pages.
pub const MAX_PAGES: usize = 16;
/// Guest-physical addresses a four-level table translates.
const GPA_LIMIT: u64 = 1 << 48;
const MIB2: u64 = 2 << 20;

/// Why a mapping was refused. A refusal may leave earlier leaves of the same call in place;
/// they map memory the table owns, so that is untidy, not unsafe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    /// Not 4 KiB aligned.
    Unaligned,
    /// Past the RAM run, or past the guest-physical limit.
    OutOfRange,
    /// Something is already mapped there.
    Occupied,
    /// No frame for a table.
    NoFrame,
    /// [`MAX_TABLES`], [`MAX_RAM`] or [`MAX_PAGES`] reached.
    Full,
}

/// An adopted RAM run, by position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RamId(usize);

#[derive(Clone, Copy)]
enum Table {
    Root,
    Owned(usize),
}

/// A nested page table and everything it maps.
pub struct Npt {
    pml4: FreshFrame,
    tables: [Option<FreshFrame>; MAX_TABLES],
    ram: [Option<GuestRam>; MAX_RAM],
    pages: [Option<FreshFrame>; MAX_PAGES],
}

impl Npt {
    /// An empty table: every guest-physical access faults (`#VMEXIT(NPF)`).
    #[must_use]
    pub fn new() -> Option<Self> {
        Some(Self {
            pml4: FreshFrame::alloc_zeroed()?,
            tables: [const { None }; MAX_TABLES],
            ram: [const { None }; MAX_RAM],
            pages: [const { None }; MAX_PAGES],
        })
    }

    /// The PML4's physical address: what `N_CR3` is set to at entry.
    #[must_use]
    pub fn root(&self) -> PhysAddr {
        self.pml4.pa()
    }

    /// Take `ram` for the table's lifetime; [`Npt::map_ram`] can then map it.
    ///
    /// # Errors
    /// `ram` back, when [`MAX_RAM`] runs are already held.
    pub fn adopt_ram(&mut self, ram: GuestRam) -> Result<RamId, GuestRam> {
        match self.ram.iter().position(Option::is_none) {
            Some(i) => {
                self.ram[i] = Some(ram);
                Ok(RamId(i))
            }
            None => Err(ram),
        }
    }

    /// An adopted run, for the host's own copies in and out.
    #[must_use]
    pub fn ram(&self, id: RamId) -> Option<&GuestRam> {
        self.ram.get(id.0)?.as_ref()
    }

    /// Map `bytes` of run `id` from `offset` at guest-physical `gpa`, read-write-execute:
    /// 2 MiB leaves where both sides are 2 MiB aligned, 4 KiB leaves elsewhere.
    ///
    /// # Errors
    /// [`MapError`]; `Occupied` if any page of the range is already mapped.
    pub fn map_ram(&mut self, id: RamId, offset: u64, bytes: u64, gpa: u64) -> Result<(), MapError> {
        let (pa, len) = {
            let r = self.ram(id).ok_or(MapError::OutOfRange)?;
            (r.pa(), r.bytes())
        };
        if !(offset | bytes | gpa).is_multiple_of(4096) {
            return Err(MapError::Unaligned);
        }
        if offset.checked_add(bytes).is_none_or(|e| e > len) || gpa.checked_add(bytes).is_none_or(|e| e > GPA_LIMIT) {
            return Err(MapError::OutOfRange);
        }
        let mut done = 0;
        while done < bytes {
            let (g, h) = (gpa + done, pa + offset + done);
            if g.is_multiple_of(MIB2) && h.is_multiple_of(MIB2) && bytes - done >= MIB2 {
                self.leaf(g, leaf_2m(h), true)?;
                done += MIB2;
            } else {
                self.leaf(g, leaf_4k(h), false)?;
                done += 4096;
            }
        }
        Ok(())
    }

    /// Take `page` for the table's lifetime and map it at guest-physical `gpa` (a 4 KiB leaf).
    /// On a refusal the page is dropped, which frees it.
    ///
    /// # Errors
    /// [`MapError`].
    pub fn adopt_page(&mut self, page: FreshFrame, gpa: u64) -> Result<(), MapError> {
        if !gpa.is_multiple_of(4096) {
            return Err(MapError::Unaligned);
        }
        if gpa >= GPA_LIMIT {
            return Err(MapError::OutOfRange);
        }
        let slot = self.pages.iter().position(Option::is_none).ok_or(MapError::Full)?;
        self.leaf(gpa, leaf_4k(page.pa().get()), false)?;
        self.pages[slot] = Some(page);
        Ok(())
    }

    fn bytes(&mut self, t: Table) -> &mut [u8; 4096] {
        match t {
            Table::Root => self.pml4.bytes_mut(),
            // `Owned(i)` is only ever made for a filled slot (`child`).
            Table::Owned(i) => self.tables[i].as_mut().map_or_else(|| unreachable!(), FreshFrame::bytes_mut),
        }
    }

    /// The table entry `idx` of `t` points at, made if absent.
    fn child(&mut self, t: Table, idx: usize) -> Result<Table, MapError> {
        let e = rd64(self.bytes(t), idx * 8);
        if e & P != 0 {
            if e & PS != 0 {
                return Err(MapError::Occupied);
            }
            let pa = e & ADDR_MASK;
            // Every pointer this type writes names a table it owns.
            return self
                .tables
                .iter()
                .position(|f| f.as_ref().is_some_and(|f| f.pa().get() == pa))
                .map(Table::Owned)
                .ok_or(MapError::Occupied);
        }
        let slot = self.tables.iter().position(Option::is_none).ok_or(MapError::Full)?;
        let f = FreshFrame::alloc_zeroed().ok_or(MapError::NoFrame)?;
        let pa = f.pa().get();
        self.tables[slot] = Some(f);
        wr64(self.bytes(t), idx * 8, pointer(pa));
        Ok(Table::Owned(slot))
    }

    /// Write leaf `entry` for `gpa`: in a directory if `big`, else in a page table.
    fn leaf(&mut self, gpa: u64, entry: u64, big: bool) -> Result<(), MapError> {
        let ix = |shift: u32| ((gpa >> shift) & 0x1FF) as usize;
        let pdpt = self.child(Table::Root, ix(39))?;
        let pd = self.child(pdpt, ix(30))?;
        let (t, i) = if big { (pd, ix(21)) } else { (self.child(pd, ix(21))?, ix(12)) };
        if rd64(self.bytes(t), i * 8) & P != 0 {
            return Err(MapError::Occupied);
        }
        wr64(self.bytes(t), i * 8, entry);
        Ok(())
    }
}

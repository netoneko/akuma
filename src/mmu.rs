//! MMU (Memory Management Unit) for AArch64
//!
//! Implements page table management for virtual memory.
//! Uses 4KB granule with 4-level page tables (L0-L3).
//!
//! Memory layout:
//! - TTBR0_EL1: User space (0x0000_0000_0000_0000 - 0x0000_FFFF_FFFF_FFFF)
//! - TTBR1_EL1: Kernel space (0xFFFF_0000_0000_0000 - 0xFFFF_FFFF_FFFF_FFFF)
//!
//! The kernel runs in the upper half (TTBR1) so that TTBR0 can be switched
//! per-process for user space mappings.

#![allow(dead_code)]

use core::sync::atomic::{AtomicBool, Ordering};

/// Page size: 4KB
pub const PAGE_SIZE: usize = 4096;
pub const PAGE_SHIFT: usize = 12;

/// Page table entry count per level
pub const ENTRIES_PER_TABLE: usize = 512;

/// Virtual address bits per level
pub const BITS_PER_LEVEL: usize = 9;

/// Memory attribute indices (configured in MAIR_EL1)
pub const MAIR_DEVICE_NGNRNE: u64 = 0; // Device memory, non-Gathering, non-Reordering, non-Early Write Acknowledgement
pub const MAIR_NORMAL_NC: u64 = 1; // Normal memory, non-cacheable
pub const MAIR_NORMAL_WT: u64 = 2; // Normal memory, write-through
pub const MAIR_NORMAL_WB: u64 = 3; // Normal memory, write-back

/// Page table entry flags
pub mod flags {
    /// Entry is valid
    pub const VALID: u64 = 1 << 0;
    /// Table descriptor (vs block descriptor)
    pub const TABLE: u64 = 1 << 1;
    /// Block descriptor for L1/L2 (1GB/2MB blocks)
    pub const BLOCK: u64 = 0 << 1;
    /// Access flag (must be set or access fault)
    pub const AF: u64 = 1 << 10;
    /// Shareability: Inner shareable
    pub const SH_INNER: u64 = 3 << 8;
    /// Shareability: Outer shareable
    pub const SH_OUTER: u64 = 2 << 8;
    /// AP[2:1] - Access permissions
    pub const AP_RW_EL1: u64 = 0 << 6; // R/W at EL1, no access at EL0
    pub const AP_RW_ALL: u64 = 1 << 6; // R/W at EL1 and EL0
    pub const AP_RO_EL1: u64 = 2 << 6; // R/O at EL1, no access at EL0
    pub const AP_RO_ALL: u64 = 3 << 6; // R/O at EL1 and EL0
    /// User accessible (EL0)
    pub const USER: u64 = 1 << 6;
    /// Execute never at EL1
    pub const PXN: u64 = 1 << 53;
    /// Execute never at EL0
    pub const UXN: u64 = 1 << 54;
    /// Non-global (uses ASID)
    pub const NG: u64 = 1 << 11;
}

/// Memory attribute index in entry (bits 4:2)
#[inline]
pub const fn attr_index(idx: u64) -> u64 {
    (idx & 0x7) << 2
}

/// 1GB block size
pub const BLOCK_1GB: usize = 1 << 30;
/// 2MB block size
pub const BLOCK_2MB: usize = 1 << 21;

/// Kernel page tables (L0, L1) - statically allocated
/// We use 1GB blocks at L1 level for kernel identity mapping
#[repr(C, align(4096))]
pub struct PageTable {
    entries: [u64; ENTRIES_PER_TABLE],
}

impl PageTable {
    pub const fn new() -> Self {
        Self {
            entries: [0; ENTRIES_PER_TABLE],
        }
    }
}

/// MMU initialization state
static MMU_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Check if MMU is initialized
pub fn is_initialized() -> bool {
    MMU_INITIALIZED.load(Ordering::Acquire)
}

/// Mark MMU as initialized
pub fn init(_ram_base: usize, _ram_size: usize) {
    MMU_INITIALIZED.store(true, Ordering::Release);
}

// =============================================================================
// Physical/Virtual Address Translation
// =============================================================================

#[inline(always)]
pub fn phys_to_virt(paddr: usize) -> *mut u8 {
    paddr as *mut u8
}

#[inline(always)]
pub fn virt_to_phys(vaddr: usize) -> usize {
    vaddr
}

pub fn flush_tlb_all() {
    unsafe {
        core::arch::asm!("dsb ishst", "tlbi vmalle1", "dsb ish", "isb");
    }
}

pub fn get_boot_ttbr0() -> u64 {
    unsafe {
        let addr: u64;
        core::arch::asm!(
            "adrp {tmp}, boot_ttbr0_addr",
            "add {tmp}, {tmp}, :lo12:boot_ttbr0_addr",
            "ldr {out}, [{tmp}]",
            tmp = out(reg) _,
            out = out(reg) addr,
        );
        addr
    }
}

pub fn flush_tlb_asid(asid: u16) {
    unsafe {
        let asid_val = (asid as u64) << 48;
        core::arch::asm!(
            "dsb ishst",
            "tlbi aside1, {}",
            "dsb ish",
            "isb",
            in(reg) asid_val
        );
    }
}

pub fn flush_tlb_page(va: usize) {
    unsafe {
        let va_shifted = (va >> 12) as u64;
        core::arch::asm!(
            "dsb ishst",
            "tlbi vaae1, {}",
            "dsb ish",
            "isb",
            in(reg) va_shifted
        );
    }
}

// ============================================================================
// User Address Space Management
// ============================================================================

use alloc::vec::Vec;
use spinning_top::Spinlock;

use crate::pmm::{self, PhysFrame};

const MAX_ASID: u16 = 256;
static ASID_ALLOCATOR: Spinlock<AsidAllocator> = Spinlock::new(AsidAllocator::new());

struct AsidAllocator {
    next_asid: u16,
    used: [u64; 4],
}

impl AsidAllocator {
    const fn new() -> Self {
        Self {
            next_asid: 1,
            used: [0; 4],
        }
    }

    fn alloc(&mut self) -> Option<u16> {
        let start = self.next_asid;
        let mut asid = start;
        loop {
            let word = (asid / 64) as usize;
            let bit = asid % 64;
            if word < self.used.len() && (self.used[word] & (1 << bit)) == 0 {
                self.used[word] |= 1 << bit;
                self.next_asid = if asid + 1 >= MAX_ASID { 1 } else { asid + 1 };
                return Some(asid);
            }
            asid = if asid + 1 >= MAX_ASID { 1 } else { asid + 1 };
            if asid == start { return None; }
        }
    }

    fn free(&mut self, asid: u16) {
        if asid > 0 && asid < MAX_ASID {
            let word = (asid / 64) as usize;
            let bit = asid % 64;
            if word < self.used.len() { self.used[word] &= !(1 << bit); }
        }
    }
}

pub struct UserAddressSpace {
    l0_frame: PhysFrame,
    page_table_frames: Vec<PhysFrame>,
    user_frames: Vec<PhysFrame>,
    asid: u16,
    shared: bool,
}

impl UserAddressSpace {
    pub fn new() -> Option<Self> {
        let l0_frame = pmm::alloc_page_zeroed()?;
        pmm::track_frame(l0_frame, pmm::FrameSource::UserPageTable, 0);
        let asid = crate::irq::with_irqs_disabled(|| ASID_ALLOCATOR.lock().alloc())?;
        let mut addr_space = Self {
            l0_frame,
            page_table_frames: Vec::new(),
            user_frames: Vec::new(),
            asid,
            shared: false,
        };
        addr_space.add_kernel_mappings().ok()?;
        Some(addr_space)
    }

    /// Create a shared view of an existing address space (for CLONE_THREAD).
    /// Uses the same L0 page table; Drop will NOT free the pages.
    pub fn new_shared(parent_l0_phys: usize) -> Option<Self> {
        let asid = crate::irq::with_irqs_disabled(|| ASID_ALLOCATOR.lock().alloc())?;
        Some(Self {
            l0_frame: PhysFrame { addr: parent_l0_phys },
            page_table_frames: Vec::new(),
            user_frames: Vec::new(),
            asid,
            shared: true,
        })
    }

    fn add_kernel_mappings(&mut self) -> Result<(), &'static str> {
        let l1_frame = pmm::alloc_page_zeroed().ok_or("Failed to allocate L1 table")?;
        pmm::track_frame(l1_frame, pmm::FrameSource::UserPageTable, 0);
        self.page_table_frames.push(l1_frame);

        let l0_ptr = phys_to_virt(self.l0_frame.addr) as *mut u64;
        unsafe {
            let l1_entry = (l1_frame.addr as u64) | flags::VALID | flags::TABLE;
            core::ptr::write_volatile(l0_ptr, l1_entry);
        }

        let l1_ptr = phys_to_virt(l1_frame.addr) as *mut u64;
        let l2_frame = pmm::alloc_page_zeroed().ok_or("Failed to allocate L2 table")?;
        pmm::track_frame(l2_frame, pmm::FrameSource::UserPageTable, 0);
        self.page_table_frames.push(l2_frame);

        unsafe {
            let l2_entry = (l2_frame.addr as u64) | flags::VALID | flags::TABLE;
            core::ptr::write_volatile(l1_ptr.add(0), l2_entry);
        }

        let l2_ptr = phys_to_virt(l2_frame.addr) as *mut u64;
        let device_block_flags = flags::VALID | flags::BLOCK | flags::AF | attr_index(MAIR_DEVICE_NGNRNE) | flags::PXN | flags::UXN | flags::SH_OUTER;

        for i in 64..96 {
            let pa = (i as u64) * 0x200000;
            unsafe { core::ptr::write_volatile(l2_ptr.add(i), pa | device_block_flags); }
        }

        // L1[1]: kernel RAM. Use an L2 table with 2MB blocks for only the
        // actual 256MB of RAM (128 blocks) instead of a 1GB L1 block.
        // This leaves VA 0x50000000-0x7FFFFFFF available for user mmap.
        let l2_ram_frame = pmm::alloc_page_zeroed().ok_or("Failed to allocate kernel RAM L2 table")?;
        pmm::track_frame(l2_ram_frame, pmm::FrameSource::UserPageTable, 0);
        self.page_table_frames.push(l2_ram_frame);

        unsafe {
            let l2_ram_entry = (l2_ram_frame.addr as u64) | flags::VALID | flags::TABLE;
            core::ptr::write_volatile(l1_ptr.add(1), l2_ram_entry);

            let l2_ram_ptr = phys_to_virt(l2_ram_frame.addr) as *mut u64;
            let kernel_ram_flags = flags::VALID | flags::BLOCK | flags::AF
                | attr_index(MAIR_NORMAL_WB) | flags::UXN | flags::SH_INNER | (0b00 << 6);

            // 256MB = 128 × 2MB blocks: VA 0x40000000-0x4FFFFFFF → PA 0x40000000-0x4FFFFFFF
            for i in 0..128u64 {
                let pa = 0x4000_0000 + i * 0x20_0000;
                core::ptr::write_volatile(l2_ram_ptr.add(i as usize), pa | kernel_ram_flags);
            }
            // L2[128..511] left zeroed — VA 0x50000000-0x7FFFFFFF available for user pages
        }
        Ok(())
    }

    pub fn ttbr0(&self) -> u64 {
        ((self.asid as u64) << 48) | (self.l0_frame.addr as u64)
    }

    pub fn l0_phys(&self) -> usize { self.l0_frame.addr }

    pub fn is_shared(&self) -> bool { self.shared }

    pub fn asid(&self) -> u16 { self.asid }

    pub fn map_page(&mut self, va: usize, pa: usize, user_flags: u64) -> Result<(), &'static str> {
        if va & (PAGE_SIZE - 1) != 0 || pa & (PAGE_SIZE - 1) != 0 { return Err("Addresses must be page-aligned"); }
        let l0_idx = (va >> 39) & 0x1FF;
        let l1_idx = (va >> 30) & 0x1FF;
        let l2_idx = (va >> 21) & 0x1FF;
        let l3_idx = (va >> 12) & 0x1FF;

        let l0_ptr = phys_to_virt(self.l0_frame.addr) as *mut u64;
        let l1_frame = self.get_or_create_table(l0_ptr, l0_idx)?;
        let l1_ptr = phys_to_virt(l1_frame.addr) as *mut u64;
        let l2_frame = self.get_or_create_table(l1_ptr, l1_idx)?;
        let l2_ptr = phys_to_virt(l2_frame.addr) as *mut u64;
        let l3_frame = self.get_or_create_table(l2_ptr, l2_idx)?;
        let l3_ptr = phys_to_virt(l3_frame.addr) as *mut u64;

        let entry = (pa as u64) | flags::VALID | flags::TABLE | flags::AF | flags::NG | attr_index(MAIR_NORMAL_WB) | flags::SH_INNER | user_flags;
        unsafe { l3_ptr.add(l3_idx).write_volatile(entry); }
        Ok(())
    }

    fn get_or_create_table(&mut self, table_ptr: *mut u64, idx: usize) -> Result<PhysFrame, &'static str> {
        unsafe {
            let entry = table_ptr.add(idx).read_volatile();
            if entry & flags::VALID != 0 {
                if entry & flags::TABLE == 0 {
                    // BLOCK descriptor occupies this slot — replace with a table.
                    // The block mapped nonexistent or kernel-only memory that we
                    // now need to carve into per-page mappings for user space.
                    let frame = pmm::alloc_page_zeroed().ok_or("Out of memory for page table")?;
                    pmm::track_frame(frame, pmm::FrameSource::UserPageTable, 0);
                    self.page_table_frames.push(frame);
                    let new_entry = (frame.addr as u64) | flags::VALID | flags::TABLE;
                    table_ptr.add(idx).write_volatile(new_entry);
                    Ok(frame)
                } else {
                    Ok(PhysFrame::new((entry & 0x0000_FFFF_FFFF_F000) as usize))
                }
            } else {
                let frame = pmm::alloc_page_zeroed().ok_or("Out of memory for page table")?;
                pmm::track_frame(frame, pmm::FrameSource::UserPageTable, 0);
                self.page_table_frames.push(frame);
                let new_entry = (frame.addr as u64) | flags::VALID | flags::TABLE;
                table_ptr.add(idx).write_volatile(new_entry);
                Ok(frame)
            }
        }
    }

    pub fn map_range(&mut self, va_start: usize, pa_start: usize, size: usize, user_flags: u64) -> Result<(), &'static str> {
        let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
        for i in 0..pages {
            self.map_page(va_start + i * PAGE_SIZE, pa_start + i * PAGE_SIZE, user_flags)?;
        }
        Ok(())
    }

    pub fn alloc_and_map(&mut self, va: usize, user_flags: u64) -> Result<PhysFrame, &'static str> {
        let frame = pmm::alloc_page_zeroed().ok_or("Out of memory for user page")?;
        pmm::track_frame(frame, pmm::FrameSource::ElfLoader, 0);
        self.user_frames.push(frame);
        self.map_page(va, frame.addr, user_flags)?;
        Ok(frame)
    }

    pub fn is_range_mapped(&self, va_start: usize, len: usize) -> bool {
        if len == 0 { return true; }
        let start_page = va_start & !(PAGE_SIZE - 1);
        let end_page = (va_start + len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let num_pages = (end_page - start_page) / PAGE_SIZE;
        let l0_ptr = phys_to_virt(self.l0_frame.addr) as *const u64;
        for i in 0..num_pages {
            if !self.is_page_mapped(l0_ptr, start_page + i * PAGE_SIZE) { return false; }
        }
        true
    }

    fn is_page_mapped(&self, l0_ptr: *const u64, va: usize) -> bool {
        let l0_idx = (va >> 39) & 0x1FF;
        let l1_idx = (va >> 30) & 0x1FF;
        let l2_idx = (va >> 21) & 0x1FF;
        let l3_idx = (va >> 12) & 0x1FF;
        unsafe {
            let l0_entry = l0_ptr.add(l0_idx).read_volatile();
            if l0_entry & flags::VALID == 0 { return false; }
            let l1_ptr = phys_to_virt((l0_entry & 0x0000_FFFF_FFFF_F000) as usize) as *const u64;
            let l1_entry = l1_ptr.add(l1_idx).read_volatile();
            if l1_entry & flags::VALID == 0 { return false; }
            if l1_entry & flags::TABLE == 0 { return true; }
            let l2_ptr = phys_to_virt((l1_entry & 0x0000_FFFF_FFFF_F000) as usize) as *const u64;
            let l2_entry = l2_ptr.add(l2_idx).read_volatile();
            if l2_entry & flags::VALID == 0 { return false; }
            if l2_entry & flags::TABLE == 0 { return true; }
            let l3_ptr = phys_to_virt((l2_entry & 0x0000_FFFF_FFFF_F000) as usize) as *const u64;
            let l3_entry = l3_ptr.add(l3_idx).read_volatile();
            l3_entry & flags::VALID != 0
        }
    }

    pub fn track_user_frame(&mut self, frame: PhysFrame) { self.user_frames.push(frame); }
    pub fn track_page_table_frame(&mut self, frame: PhysFrame) { self.page_table_frames.push(frame); }
    pub fn remove_user_frame(&mut self, frame: PhysFrame) {
        if let Some(idx) = self.user_frames.iter().position(|f| f.addr == frame.addr) { self.user_frames.swap_remove(idx); }
    }

    pub fn unmap_page(&mut self, va: usize) -> Result<(), &'static str> {
        let l0_idx = (va >> 39) & 0x1FF;
        let l1_idx = (va >> 30) & 0x1FF;
        let l2_idx = (va >> 21) & 0x1FF;
        let l3_idx = (va >> 12) & 0x1FF;
        unsafe {
            let l0_ptr = phys_to_virt(self.l0_frame.addr) as *mut u64;
            let l0_entry = l0_ptr.add(l0_idx).read_volatile();
            if l0_entry & flags::VALID == 0 { return Ok(()); }
            let l1_ptr = phys_to_virt((l0_entry & 0x0000_FFFF_FFFF_F000) as usize) as *mut u64;
            let l1_entry = l1_ptr.add(l1_idx).read_volatile();
            if l1_entry & flags::VALID == 0 { return Ok(()); }
            let l2_ptr = phys_to_virt((l1_entry & 0x0000_FFFF_FFFF_F000) as usize) as *mut u64;
            let l2_entry = l2_ptr.add(l2_idx).read_volatile();
            if l2_entry & flags::VALID == 0 { return Ok(()); }
            let l3_ptr = phys_to_virt((l2_entry & 0x0000_FFFF_FFFF_F000) as usize) as *mut u64;
            l3_ptr.add(l3_idx).write_volatile(0);
        }
        flush_tlb_page(va);
        Ok(())
    }

    /// Update the permission bits of an existing L3 page table entry.
    /// Preserves the physical address and fixed flags, replaces only user permission bits.
    pub fn update_page_flags(&mut self, va: usize, new_flags: u64) -> Result<(), &'static str> {
        let l0_idx = (va >> 39) & 0x1FF;
        let l1_idx = (va >> 30) & 0x1FF;
        let l2_idx = (va >> 21) & 0x1FF;
        let l3_idx = (va >> 12) & 0x1FF;
        const PERM_MASK: u64 = flags::AP_RO_ALL | flags::AP_RW_ALL | flags::UXN | flags::PXN;
        unsafe {
            let l0_ptr = phys_to_virt(self.l0_frame.addr) as *mut u64;
            let l0_entry = l0_ptr.add(l0_idx).read_volatile();
            if l0_entry & flags::VALID == 0 { return Ok(()); }
            let l1_ptr = phys_to_virt((l0_entry & 0x0000_FFFF_FFFF_F000) as usize) as *mut u64;
            let l1_entry = l1_ptr.add(l1_idx).read_volatile();
            if l1_entry & flags::VALID == 0 { return Ok(()); }
            let l2_ptr = phys_to_virt((l1_entry & 0x0000_FFFF_FFFF_F000) as usize) as *mut u64;
            let l2_entry = l2_ptr.add(l2_idx).read_volatile();
            if l2_entry & flags::VALID == 0 { return Ok(()); }
            let l3_ptr = phys_to_virt((l2_entry & 0x0000_FFFF_FFFF_F000) as usize) as *mut u64;
            let old_entry = l3_ptr.add(l3_idx).read_volatile();
            if old_entry & flags::VALID == 0 { return Ok(()); }
            let entry = (old_entry & !PERM_MASK) | new_flags;
            l3_ptr.add(l3_idx).write_volatile(entry);
        }
        flush_tlb_page(va);
        Ok(())
    }

    /// Check whether a virtual address has a valid page table entry (public).
    pub fn is_mapped(&self, va: usize) -> bool {
        let l0_ptr = phys_to_virt(self.l0_frame.addr) as *const u64;
        self.is_page_mapped(l0_ptr, va)
    }

    pub fn activate(&self) {
        let ttbr0 = self.ttbr0();
        flush_tlb_all();
        unsafe {
            core::arch::asm!("dsb ish", "msr ttbr0_el1, {ttbr0}", "isb", ttbr0 = in(reg) ttbr0);
        }
        flush_tlb_all();
    }

    pub fn deactivate() {
        let boot_ttbr0 = get_boot_ttbr0();
        flush_tlb_all();
        unsafe {
            core::arch::asm!("dsb ish", "msr ttbr0_el1, {ttbr0}", "isb", ttbr0 = in(reg) boot_ttbr0);
        }
        flush_tlb_all();
    }
}

impl Drop for UserAddressSpace {
    fn drop(&mut self) {
        if !self.shared {
            for frame in &self.user_frames { pmm::free_page(*frame); }
            for frame in &self.page_table_frames { pmm::free_page(*frame); }
            pmm::free_page(self.l0_frame);
        }
        crate::irq::with_irqs_disabled(|| ASID_ALLOCATOR.lock().free(self.asid));
        flush_tlb_asid(self.asid);
    }
}

pub mod user_flags {
    use super::flags;
    pub const RO: u64 = flags::AP_RO_ALL;
    pub const RW: u64 = flags::AP_RW_ALL;
    pub const EXEC: u64 = flags::AP_RO_ALL;
    pub const RW_NO_EXEC: u64 = flags::AP_RW_ALL | flags::UXN | flags::PXN;
    pub const RX: u64 = flags::AP_RO_ALL | flags::PXN;

    pub fn from_prot(prot: u32) -> u64 {
        match (prot & 0x2 != 0, prot & 0x4 != 0) {
            (true, _)      => RW_NO_EXEC,
            (false, true)  => RX,
            (false, false) => RO,
        }
    }
}

pub unsafe fn map_user_page(va: usize, pa: usize, user_flags_val: u64) -> Vec<PhysFrame> { unsafe {
    let mut allocated_tables = Vec::new();
    let ttbr0: u64;
    core::arch::asm!("mrs {}, TTBR0_EL1", out(reg) ttbr0);
    let l0_addr = (ttbr0 & 0x0000_FFFF_FFFF_F000) as usize;
    let l0_idx = (va >> 39) & 0x1FF;
    let l1_idx = (va >> 30) & 0x1FF;
    let l2_idx = (va >> 21) & 0x1FF;
    let l3_idx = (va >> 12) & 0x1FF;
    let l0_ptr = phys_to_virt(l0_addr) as *mut u64;
    let (l1_addr, l1_frame) = get_or_create_table_raw(l0_ptr, l0_idx);
    if let Some(frame) = l1_frame { allocated_tables.push(frame); }
    let l1_ptr = phys_to_virt(l1_addr) as *mut u64;
    let (l2_addr, l2_frame) = get_or_create_table_raw(l1_ptr, l1_idx);
    if let Some(frame) = l2_frame { allocated_tables.push(frame); }
    let l2_ptr = phys_to_virt(l2_addr) as *mut u64;
    let (l3_addr, l3_frame) = get_or_create_table_raw(l2_ptr, l2_idx);
    if let Some(frame) = l3_frame { allocated_tables.push(frame); }
    let l3_ptr = phys_to_virt(l3_addr) as *mut u64;
    let entry = (pa as u64) | flags::VALID | flags::TABLE | flags::AF | flags::NG | attr_index(MAIR_NORMAL_WB) | flags::SH_INNER | user_flags_val;
    l3_ptr.add(l3_idx).write_volatile(entry);
    core::arch::asm!("dsb ishst", "tlbi vale1is, {va}", "dsb ish", "isb", va = in(reg) va >> 12);
    allocated_tables
}}

unsafe fn get_or_create_table_raw(table_ptr: *mut u64, idx: usize) -> (usize, Option<PhysFrame>) { unsafe {
    let entry = table_ptr.add(idx).read_volatile();
    if entry & flags::VALID != 0 {
        if entry & flags::TABLE == 0 {
            // BLOCK descriptor — replace with a table so we can map individual pages
            if let Some(frame) = crate::pmm::alloc_page_zeroed() {
                let new_entry = (frame.addr as u64) | flags::VALID | flags::TABLE;
                table_ptr.add(idx).write_volatile(new_entry);
                return (frame.addr, Some(frame));
            }
            return (0, None);
        }
        ((entry & 0x0000_FFFF_FFFF_F000) as usize, None)
    } else if let Some(frame) = crate::pmm::alloc_page_zeroed() {
        let new_entry = (frame.addr as u64) | flags::VALID | flags::TABLE;
        table_ptr.add(idx).write_volatile(new_entry);
        (frame.addr, Some(frame))
    } else { (0, None) }
}}

pub fn protect_kernel_code() {
    unsafe extern "C" {
        static _text_start: u8; static _text_end: u8;
        static _rodata_start: u8; static _rodata_end: u8;
        static _data_start: u8;
        static _kernel_phys_end: u8;
    }
    let (text_start, text_end, rodata_start, rodata_end, data_start) = unsafe {
        (&_text_start as *const u8 as usize,
         &_text_end as *const u8 as usize,
         &_rodata_start as *const u8 as usize,
         &_rodata_end as *const u8 as usize,
         &_data_start as *const u8 as usize)
    };

    const BLOCK_SIZE_2MB: usize = 2 * 1024 * 1024;
    const RAM_BASE: usize = 0x40000000;
    
    let text_block_start = (text_start - RAM_BASE) / BLOCK_SIZE_2MB;
    let rodata_block_end = (rodata_end - RAM_BASE + BLOCK_SIZE_2MB - 1) / BLOCK_SIZE_2MB;
    let data_block_start = (data_start - RAM_BASE) / BLOCK_SIZE_2MB;
    let l3_block_start = text_block_start;
    let l3_block_end = if data_block_start > rodata_block_end { rodata_block_end } else { data_block_start + 1 };
    let num_l3_blocks = l3_block_end - l3_block_start;
    
    let l2_table = match crate::pmm::alloc_page() {
        Some(frame) => frame.start_address(),
        None => return,
    };
    unsafe { core::ptr::write_bytes(l2_table as *mut u8, 0, PAGE_SIZE); }
    
    let mut l3_tables: [usize; 16] = [0; 16];
    if num_l3_blocks > 16 { return; }
    for i in 0..num_l3_blocks {
        l3_tables[i] = match crate::pmm::alloc_page() {
            Some(frame) => frame.start_address(),
            None => return,
        };
        unsafe { core::ptr::write_bytes(l3_tables[i] as *mut u8, 0, PAGE_SIZE); }
    }
    
    let l2_ptr = l2_table as *mut u64;
    const BLOCK_RW: u64 = flags::VALID | (3 << 2) | flags::SH_INNER | flags::AF;
    const PAGE_RW: u64 = flags::VALID | flags::TABLE | (3 << 2) | flags::SH_INNER | flags::AF;
    const PAGE_RO: u64 = flags::VALID | flags::TABLE | (3 << 2) | flags::SH_INNER | flags::AF | flags::AP_RO_EL1;
    
    for i in 0..512 {
        let block_addr = RAM_BASE + i * BLOCK_SIZE_2MB;
        if i >= l3_block_start && i < l3_block_end {
            let l3_ptr = l3_tables[i - l3_block_start] as *mut u64;
            for j in 0..512 {
                let page_addr = block_addr + j * PAGE_SIZE;
                let is_ro = (page_addr >= text_start && page_addr < text_end) || (page_addr >= rodata_start && page_addr < rodata_end);
                unsafe { l3_ptr.add(j).write_volatile((page_addr as u64) | if is_ro { PAGE_RO } else { PAGE_RW }); }
            }
            unsafe { l2_ptr.add(i).write_volatile((l3_tables[i - l3_block_start] as u64) | flags::VALID | flags::TABLE); }
        } else {
            unsafe { l2_ptr.add(i).write_volatile((block_addr as u64) | BLOCK_RW); }
        }
    }
    
    let l0_table = get_boot_ttbr0() as *mut u64;
    unsafe {
        let l1_table = ((*l0_table) & 0x0000_FFFF_FFFF_F000) as *mut u64;
        core::arch::asm!("dsb ishst");
        l1_table.add(1).write_volatile((l2_table as u64) | flags::VALID | flags::TABLE);
        core::arch::asm!("dsb ish", "tlbi vmalle1", "dsb ish", "isb");
    }
}

pub fn get_current_ttbr0() -> usize {
    let ttbr0: u64;
    unsafe { core::arch::asm!("mrs {}, ttbr0_el1", out(reg) ttbr0); }
    ttbr0 as usize
}

pub fn is_current_user_range_mapped(va_start: usize, len: usize) -> bool {
    let ttbr0 = get_current_ttbr0();
    if ttbr0 == 0 { return false; }
    let l0_addr = ttbr0 & 0x0000_FFFF_FFFF_F000;
    let start_page = va_start & !(PAGE_SIZE - 1);
    let end_page = (va_start + len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let num_pages = (end_page - start_page) / PAGE_SIZE;
    let l0_ptr = phys_to_virt(l0_addr) as *const u64;
    for i in 0..num_pages {
        if !is_page_mapped_ptr(l0_ptr, start_page + i * PAGE_SIZE) { return false; }
    }
    true
}

/// Translate a user VA to its physical address using the given L0 page table.
/// Returns None if the page is not mapped.
pub fn translate_user_va(l0_ptr: *const u64, va: usize) -> Option<usize> {
    let l0_idx = (va >> 39) & 0x1FF;
    let l1_idx = (va >> 30) & 0x1FF;
    let l2_idx = (va >> 21) & 0x1FF;
    let l3_idx = (va >> 12) & 0x1FF;
    let offset = va & 0xFFF;
    unsafe {
        let l0_entry = l0_ptr.add(l0_idx).read_volatile();
        if l0_entry & flags::VALID == 0 { return None; }
        let l1_ptr = phys_to_virt((l0_entry & 0x0000_FFFF_FFFF_F000) as usize) as *const u64;
        let l1_entry = l1_ptr.add(l1_idx).read_volatile();
        if l1_entry & flags::VALID == 0 { return None; }
        if l1_entry & flags::TABLE == 0 {
            return Some(((l1_entry & 0x0000_FFFF_C000_0000) as usize) | (va & 0x3FFF_FFFF));
        }
        let l2_ptr = phys_to_virt((l1_entry & 0x0000_FFFF_FFFF_F000) as usize) as *const u64;
        let l2_entry = l2_ptr.add(l2_idx).read_volatile();
        if l2_entry & flags::VALID == 0 { return None; }
        if l2_entry & flags::TABLE == 0 {
            return Some(((l2_entry & 0x0000_FFFF_FFE0_0000) as usize) | (va & 0x1F_FFFF));
        }
        let l3_ptr = phys_to_virt((l2_entry & 0x0000_FFFF_FFFF_F000) as usize) as *const u64;
        let l3_entry = l3_ptr.add(l3_idx).read_volatile();
        if l3_entry & flags::VALID == 0 { return None; }
        Some(((l3_entry & 0x0000_FFFF_FFFF_F000) as usize) | offset)
    }
}

fn is_page_mapped_ptr(l0_ptr: *const u64, va: usize) -> bool {
    let l0_idx = (va >> 39) & 0x1FF;
    let l1_idx = (va >> 30) & 0x1FF;
    let l2_idx = (va >> 21) & 0x1FF;
    let l3_idx = (va >> 12) & 0x1FF;
    unsafe {
        let l0_entry = l0_ptr.add(l0_idx).read_volatile();
        if l0_entry & flags::VALID == 0 { return false; }
        let l1_ptr = phys_to_virt((l0_entry & 0x0000_FFFF_FFFF_F000) as usize) as *const u64;
        let l1_entry = l1_ptr.add(l1_idx).read_volatile();
        if l1_entry & flags::VALID == 0 { return false; }
        if l1_entry & flags::TABLE == 0 { return true; } 
        let l2_ptr = phys_to_virt((l1_entry & 0x0000_FFFF_FFFF_F000) as usize) as *const u64;
        let l2_entry = l2_ptr.add(l2_idx).read_volatile();
        if l2_entry & flags::VALID == 0 { return false; }
        if l2_entry & flags::TABLE == 0 { return true; } 
        let l3_ptr = phys_to_virt((l2_entry & 0x0000_FFFF_FFFF_F000) as usize) as *const u64;
        let l3_entry = l3_ptr.add(l3_idx).read_volatile();
        l3_entry & flags::VALID != 0
    }
}

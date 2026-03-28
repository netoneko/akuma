//! MMU (Memory Management Unit) for AArch64
//!
//! Implements page table management for virtual memory.
//! Uses 4KB granule with 4-level page tables (L0-L3).

#![allow(dead_code)]

pub mod types;
pub mod asid;
pub mod user_access;

pub use types::*;

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use crate::runtime::{PhysFrame, FrameSource, runtime, with_irqs_disabled, IrqGuard};

/// MMU initialization state
static MMU_INITIALIZED: AtomicBool = AtomicBool::new(false);

static RAM_BASE: AtomicUsize = AtomicUsize::new(0);
static RAM_SIZE: AtomicUsize = AtomicUsize::new(0);

/// Check if MMU is initialized
pub fn is_initialized() -> bool {
    MMU_INITIALIZED.load(Ordering::Acquire)
}

/// Mark MMU as initialized
pub fn init(ram_base: usize, ram_size: usize) {
    RAM_BASE.store(ram_base, Ordering::Release);
    RAM_SIZE.store(ram_size, Ordering::Release);
    MMU_INITIALIZED.store(true, Ordering::Release);
}

/// Physical address of the shared device L1 table (under L0[1]).
/// Allocated once by `init_shared_device_tables()`, then referenced
/// by every user address space's `add_kernel_mappings()`.
static SHARED_DEV_L1_PHYS: AtomicUsize = AtomicUsize::new(0);

/// Device physical addresses and their L3 slot indices.
const DEV_PAGES: &[(usize, usize)] = &[
    (0, 0x0800_0000), // L3[0]: GIC distributor
    (1, 0x0801_0000), // L3[1]: GIC CPU interface
    (2, 0x0900_0000), // L3[2]: UART PL011
    (3, 0x0902_0000), // L3[3]: fw_cfg
    (4, 0x0A00_0000), // L3[4]: VirtIO MMIO
];

/// Allocate the shared L1/L2/L3 device page tables that every user address
/// space will reference via L0[1].  Must be called once during kernel init,
/// after the PMM is ready.
pub fn init_shared_device_tables() {
    let rt = runtime();
    let l1 = (rt.alloc_page_zeroed)().expect("shared dev L1");
    let l2 = (rt.alloc_page_zeroed)().expect("shared dev L2");
    let l3 = (rt.alloc_page_zeroed)().expect("shared dev L3");

    let device_page_flags: u64 = flags::VALID | flags::TABLE | flags::AF
        | attr_index(MAIR_DEVICE_NGNRNE) | flags::PXN | flags::UXN | flags::SH_OUTER;

    unsafe {
        let l1_ptr = phys_to_virt(l1.addr) as *mut u64;
        let l2_ptr = phys_to_virt(l2.addr) as *mut u64;
        let l3_ptr = phys_to_virt(l3.addr) as *mut u64;

        // L1[0] -> L2
        l1_ptr.write_volatile((l2.addr as u64) | flags::VALID | flags::TABLE);
        // L2[0] -> L3
        l2_ptr.write_volatile((l3.addr as u64) | flags::VALID | flags::TABLE);

        for &(idx, pa) in DEV_PAGES {
            l3_ptr.add(idx).write_volatile((pa as u64) | device_page_flags);
        }
    }

    SHARED_DEV_L1_PHYS.store(l1.addr, Ordering::Release);
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

#[cfg(target_os = "none")]
pub fn flush_tlb_all() {
    unsafe {
        core::arch::asm!("dsb ishst", "tlbi vmalle1", "dsb ish", "isb");
    }
}

#[cfg(not(target_os = "none"))]
pub fn flush_tlb_all() {}

#[cfg(target_os = "none")]
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

#[cfg(not(target_os = "none"))]
pub fn get_boot_ttbr0() -> u64 { 0 }

#[cfg(target_os = "none")]
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

#[cfg(not(target_os = "none"))]
pub fn flush_tlb_asid(_asid: u16) {}

#[cfg(target_os = "none")]
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

#[cfg(not(target_os = "none"))]
pub fn flush_tlb_page(_va: usize) {}

// ============================================================================
// User Address Space Management
// ============================================================================

use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use spinning_top::Spinlock;


use asid::AsidAllocator;

static ASID_ALLOCATOR: Spinlock<AsidAllocator> = Spinlock::new(AsidAllocator::new());

/// Tracks shared L0 page table reference counts and deferred frame lists.
///
/// When CLONE_THREAD creates shared views of an address space, we need to
/// ensure the page tables aren't freed until the last thread exits.
/// If the owner (shared=false) drops first, its frames are stored here
/// for the last shared view to free.
struct SharedL0Entry {
    ref_count: usize,
    deferred_user_frames: Option<Vec<PhysFrame>>,
    deferred_pt_frames: Option<Vec<PhysFrame>>,
    deferred_l0: Option<PhysFrame>,
}

static SHARED_L0_TABLE: Spinlock<BTreeMap<usize, SharedL0Entry>> =
    Spinlock::new(BTreeMap::new());

pub struct UserAddressSpace {
    l0_frame: PhysFrame,
    page_table_frames: Vec<PhysFrame>,
    user_frames: Vec<PhysFrame>,
    asid: u16,
    shared: bool,
}

impl UserAddressSpace {
    pub fn new() -> Option<Self> {
        let rt = runtime();
        let l0_frame = (rt.alloc_page_zeroed)()?;
        (rt.track_frame)(l0_frame, FrameSource::UserPageTable);
        let asid = with_irqs_disabled(|| ASID_ALLOCATOR.lock().alloc())?;
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
        let asid = with_irqs_disabled(|| ASID_ALLOCATOR.lock().alloc())?;
        with_irqs_disabled(|| {
            let mut table = SHARED_L0_TABLE.lock();
            table.entry(parent_l0_phys)
                .and_modify(|e| e.ref_count += 1)
                .or_insert(SharedL0Entry {
                    ref_count: 1,
                    deferred_user_frames: None,
                    deferred_pt_frames: None,
                    deferred_l0: None,
                });
        });
        Some(Self {
            l0_frame: PhysFrame { addr: parent_l0_phys },
            page_table_frames: Vec::new(),
            user_frames: Vec::new(),
            asid,
            shared: true,
        })
    }

    fn add_kernel_mappings(&mut self) -> Result<(), &'static str> {
        let rt = runtime();
        let l1_frame = (rt.alloc_page_zeroed)().ok_or("Failed to allocate L1 table")?;
        (rt.track_frame)(l1_frame, FrameSource::UserPageTable);
        self.page_table_frames.push(l1_frame);

        let l0_ptr = phys_to_virt(self.l0_frame.addr) as *mut u64;
        unsafe {
            let l1_entry = (l1_frame.addr as u64) | flags::VALID | flags::TABLE;
            core::ptr::write_volatile(l0_ptr, l1_entry);
        }

        let l1_ptr = phys_to_virt(l1_frame.addr) as *mut u64;
        let l2_frame = (rt.alloc_page_zeroed)().ok_or("Failed to allocate L2 table")?;
        (rt.track_frame)(l2_frame, FrameSource::UserPageTable);
        self.page_table_frames.push(l2_frame);

        unsafe {
            let l2_entry = (l2_frame.addr as u64) | flags::VALID | flags::TABLE;
            core::ptr::write_volatile(l1_ptr.add(0), l2_entry);
        }

        let l2_ptr = phys_to_virt(l2_frame.addr) as *mut u64;
        let _ = l2_ptr; // L1[0]'s L2 is now empty; all devices are under L0[1].

        // L0[1] -> shared device L1 table (all devices at VA 0x80_0000_0000+).
        // These pages are shared across all user address spaces and must NOT be
        // pushed to page_table_frames (they are never freed).
        let dev_l1_phys = SHARED_DEV_L1_PHYS.load(Ordering::Acquire);
        if dev_l1_phys != 0 {
            unsafe {
                let dev_l0_entry = (dev_l1_phys as u64) | flags::VALID | flags::TABLE;
                core::ptr::write_volatile(l0_ptr.add(1), dev_l0_entry);
            }
        }

        // Identity-map the full RAM range.
        // Use L2 tables with 2MB blocks covering the full RAM size, so that
        // user MAP_FIXED in this range can shatter individual blocks.
        // The full RAM range must be identity-mapped so that phys_to_virt()
        // works for any PMM-allocated page regardless of which TTBR0 is active.
        let ram_base = RAM_BASE.load(Ordering::Acquire);
        let ram_size = RAM_SIZE.load(Ordering::Acquire);
        let ram_end = ram_base + ram_size;

        if ram_size > 0 {
            let kernel_ram_flags = flags::VALID | flags::BLOCK | flags::AF
                | attr_index(MAIR_NORMAL_WB) | flags::UXN | flags::SH_INNER | (0b00 << 6);

            // Calculate range of 1GB L1 entries to fill
            let start_l1_idx = (ram_base >> 30) & 0x1FF;
            let end_l1_idx = ((ram_end - 1) >> 30) & 0x1FF;

            for l1_idx in start_l1_idx..=end_l1_idx {
                let l2_ram_frame = (rt.alloc_page_zeroed)().ok_or("Failed to allocate kernel RAM L2 table")?;
                (rt.track_frame)(l2_ram_frame, FrameSource::UserPageTable);
                self.page_table_frames.push(l2_ram_frame);

                unsafe {
                    let l2_ram_entry = (l2_ram_frame.addr as u64) | flags::VALID | flags::TABLE;
                    core::ptr::write_volatile(l1_ptr.add(l1_idx), l2_ram_entry);

                    let l2_ram_ptr = phys_to_virt(l2_ram_frame.addr) as *mut u64;
                    
                    // Fill this 1GB L2 table with 2MB blocks (up to 512 blocks)
                    for i in 0..512u64 {
                        let pa = ((l1_idx as usize) << 30) | ((i as usize) << 21);
                        // Only map if this 2MB block is within the RAM range
                        if pa >= ram_base && pa < ram_end {
                            core::ptr::write_volatile(l2_ram_ptr.add(i as usize), (pa as u64) | kernel_ram_flags);
                        }
                    }
                }
            }
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
        let rt = runtime();
        unsafe {
            let entry = table_ptr.add(idx).read_volatile();
            if entry & flags::VALID != 0 {
                if entry & flags::TABLE == 0 {
                    let frame = (rt.alloc_page_zeroed)().ok_or("Out of memory for page table")?;
                    (rt.track_frame)(frame, FrameSource::UserPageTable);
                    self.page_table_frames.push(frame);
                    shatter_block_to_pages(frame.addr, entry);
                    let new_entry = (frame.addr as u64) | flags::VALID | flags::TABLE;
                    table_ptr.add(idx).write_volatile(new_entry);
                    Ok(frame)
                } else {
                    Ok(PhysFrame::new((entry & 0x0000_FFFF_FFFF_F000) as usize))
                }
            } else {
                let frame = (rt.alloc_page_zeroed)().ok_or("Out of memory for page table")?;
                (rt.track_frame)(frame, FrameSource::UserPageTable);
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
        let rt = runtime();
        let frame = (rt.alloc_page_zeroed)().ok_or("Out of memory for user page")?;
        (rt.track_frame)(frame, FrameSource::ElfLoader);
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
        let _irq_guard = IrqGuard::new();
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

    /// Unmap a page and return its physical frame, also removing it from user_frames.
    /// Returns `Some(PhysFrame)` if the page was mapped, `None` if it wasn't.
    /// The caller is responsible for freeing the returned frame via PMM.
    pub fn unmap_and_free_page(&mut self, va: usize) -> Option<PhysFrame> {
        let _irq_guard = IrqGuard::new();
        let l0_idx = (va >> 39) & 0x1FF;
        let l1_idx = (va >> 30) & 0x1FF;
        let l2_idx = (va >> 21) & 0x1FF;
        let l3_idx = (va >> 12) & 0x1FF;
        let pa = unsafe {
            let l0_ptr = phys_to_virt(self.l0_frame.addr) as *mut u64;
            let l0_entry = l0_ptr.add(l0_idx).read_volatile();
            if l0_entry & flags::VALID == 0 { return None; }
            let l1_ptr = phys_to_virt((l0_entry & 0x0000_FFFF_FFFF_F000) as usize) as *mut u64;
            let l1_entry = l1_ptr.add(l1_idx).read_volatile();
            if l1_entry & flags::VALID == 0 { return None; }
            let l2_ptr = phys_to_virt((l1_entry & 0x0000_FFFF_FFFF_F000) as usize) as *mut u64;
            let l2_entry = l2_ptr.add(l2_idx).read_volatile();
            if l2_entry & flags::VALID == 0 { return None; }
            let l3_ptr = phys_to_virt((l2_entry & 0x0000_FFFF_FFFF_F000) as usize) as *mut u64;
            let l3_entry = l3_ptr.add(l3_idx).read_volatile();
            if l3_entry & flags::VALID == 0 { return None; }
            l3_ptr.add(l3_idx).write_volatile(0);
            (l3_entry & 0x0000_FFFF_FFFF_F000) as usize
        };
        flush_tlb_page(va);
        let frame = PhysFrame::new(pa);
        self.remove_user_frame(frame);
        Some(frame)
    }

    /// Zero the physical page backing `va` without unmapping it.
    /// Returns true if a page was found and zeroed, false if no mapping exists.
    pub fn zero_mapped_page(&self, va: usize) -> bool {
        let _irq_guard = IrqGuard::new();
        let l0_idx = (va >> 39) & 0x1FF;
        let l1_idx = (va >> 30) & 0x1FF;
        let l2_idx = (va >> 21) & 0x1FF;
        let l3_idx = (va >> 12) & 0x1FF;
        unsafe {
            let l0_ptr = phys_to_virt(self.l0_frame.addr) as *mut u64;
            let l0_entry = l0_ptr.add(l0_idx).read_volatile();
            if l0_entry & flags::VALID == 0 { return false; }
            let l1_ptr = phys_to_virt((l0_entry & 0x0000_FFFF_FFFF_F000) as usize) as *mut u64;
            let l1_entry = l1_ptr.add(l1_idx).read_volatile();
            if l1_entry & flags::VALID == 0 { return false; }
            let l2_ptr = phys_to_virt((l1_entry & 0x0000_FFFF_FFFF_F000) as usize) as *mut u64;
            let l2_entry = l2_ptr.add(l2_idx).read_volatile();
            if l2_entry & flags::VALID == 0 { return false; }
            let l3_ptr = phys_to_virt((l2_entry & 0x0000_FFFF_FFFF_F000) as usize) as *mut u64;
            let l3_entry = l3_ptr.add(l3_idx).read_volatile();
            if l3_entry & flags::VALID == 0 { return false; }
            let pa = (l3_entry & 0x0000_FFFF_FFFF_F000) as usize;
            core::ptr::write_bytes(phys_to_virt(pa) as *mut u8, 0, 4096);
        }
        true
    }

    /// Update the permission bits of an existing L3 page table entry.
    /// Preserves the physical address and fixed flags, replaces only user permission bits.
    pub fn update_page_flags(&mut self, va: usize, new_flags: u64) -> Result<(), &'static str> {
        let _irq_guard = IrqGuard::new();
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

    /// Same as `update_page_flags` but skips the TLB flush.
    ///
    /// Use when updating a large range of pages (e.g. mprotect over many pages).
    /// After calling this for all pages, issue a single `flush_tlb_range` or
    /// `flush_tlb_asid` to make the permission changes visible to userspace.
    pub fn update_page_flags_no_flush(&mut self, va: usize, new_flags: u64) -> Result<(), &'static str> {
        let _irq_guard = IrqGuard::new();
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
        // No TLB flush — caller must call flush_tlb_range after the batch.
        Ok(())
    }

    /// Raw L3 page descriptor for `va` (4KiB-aligned), if mapped at the final level.
    /// Used by kernel tests and diagnostics (e.g. verify `UXN` after `update_page_flags`).
    pub fn read_l3_page_entry(&self, va: usize) -> Option<u64> {
        let va = va & !(PAGE_SIZE - 1);
        let _irq_guard = IrqGuard::new();
        let l0_idx = (va >> 39) & 0x1FF;
        let l1_idx = (va >> 30) & 0x1FF;
        let l2_idx = (va >> 21) & 0x1FF;
        let l3_idx = (va >> 12) & 0x1FF;
        unsafe {
            let l0_ptr = phys_to_virt(self.l0_frame.addr) as *mut u64;
            let l0_entry = l0_ptr.add(l0_idx).read_volatile();
            if l0_entry & flags::VALID == 0 {
                return None;
            }
            let l1_ptr = phys_to_virt((l0_entry & 0x0000_FFFF_FFFF_F000) as usize) as *mut u64;
            let l1_entry = l1_ptr.add(l1_idx).read_volatile();
            if l1_entry & flags::VALID == 0 {
                return None;
            }
            let l2_ptr = phys_to_virt((l1_entry & 0x0000_FFFF_FFFF_F000) as usize) as *mut u64;
            let l2_entry = l2_ptr.add(l2_idx).read_volatile();
            if l2_entry & flags::VALID == 0 {
                return None;
            }
            let l3_ptr = phys_to_virt((l2_entry & 0x0000_FFFF_FFFF_F000) as usize) as *mut u64;
            let l3_entry = l3_ptr.add(l3_idx).read_volatile();
            if l3_entry & flags::VALID == 0 {
                return None;
            }
            Some(l3_entry)
        }
    }

    /// Physical address of the 4KiB frame backing `va`, if mapped.
    pub fn phys_addr_for_page_va(&self, va: usize) -> Option<usize> {
        let e = self.read_l3_page_entry(va)?;
        Some((e & 0x0000_FFFF_FFFF_F000) as usize)
    }

    /// Invalidate the instruction cache for the physical page backing `va`
    /// (after PTE permission changes or new code bytes). Matches the
    /// `dc cvau`/`ic ivau` pattern used when demand-paging file-backed text.
    pub fn invalidate_icache_for_page_va(&self, va: usize) {
        let Some(pa) = self.phys_addr_for_page_va(va) else {
            return;
        };
        let kva = phys_to_virt(pa) as usize;
        #[cfg(target_os = "none")]
        unsafe {
            for off in (0..PAGE_SIZE).step_by(64) {
                core::arch::asm!("ic ivau, {}", in(reg) kva + off);
            }
            core::arch::asm!("dsb ish");
            core::arch::asm!("isb");
        }
        #[cfg(not(target_os = "none"))]
        let _ = kva;
    }

    /// Check whether a virtual address has a valid page table entry (public).
    pub fn is_mapped(&self, va: usize) -> bool {
        let l0_ptr = phys_to_virt(self.l0_frame.addr) as *const u64;
        self.is_page_mapped(l0_ptr, va)
    }

    pub fn activate(&self) {
        let _ttbr0 = self.ttbr0();
        flush_tlb_all();
        #[cfg(target_os = "none")]
        unsafe {
            core::arch::asm!("dsb ish", "msr ttbr0_el1, {ttbr0}", "isb", ttbr0 = in(reg) _ttbr0);
        }
        flush_tlb_all();
    }

    pub fn deactivate() {
        let _boot_ttbr0 = get_boot_ttbr0();
        flush_tlb_all();
        #[cfg(target_os = "none")]
        unsafe {
            core::arch::asm!("dsb ish", "msr ttbr0_el1, {ttbr0}", "isb", ttbr0 = in(reg) _boot_ttbr0);
        }
        flush_tlb_all();
    }
}

impl Drop for UserAddressSpace {
    fn drop(&mut self) {
        let l0_addr = self.l0_frame.addr;
        if !self.shared {
            // Owner dropping — check if shared views still exist
            let has_shared = with_irqs_disabled(|| {
                let table = SHARED_L0_TABLE.lock();
                table.get(&l0_addr).is_some_and(|e| e.ref_count > 0)
            });
            if has_shared {
                // #region agent log
                log::debug!("[FORK-DBG] AS owner L0=0x{:x} DEFERRING free (siblings alive)", l0_addr);
                // #endregion
                let user_frames = core::mem::take(&mut self.user_frames);
                let pt_frames = core::mem::take(&mut self.page_table_frames);
                let l0 = self.l0_frame;
                with_irqs_disabled(|| {
                    let mut table = SHARED_L0_TABLE.lock();
                    if let Some(entry) = table.get_mut(&l0_addr) {
                        entry.deferred_user_frames = Some(user_frames);
                        entry.deferred_pt_frames = Some(pt_frames);
                        entry.deferred_l0 = Some(l0);
                    }
                });
            } else {
                // No shared views (or all already dropped) — free immediately
                let rt = runtime();
                for frame in &self.user_frames { (rt.free_page)(*frame); }
                for frame in &self.page_table_frames { (rt.free_page)(*frame); }
                (rt.free_page)(self.l0_frame);
                with_irqs_disabled(|| { SHARED_L0_TABLE.lock().remove(&l0_addr); });
            }
        } else {
            // Shared view dropping — decrement refcount
            let deferred = with_irqs_disabled(|| {
                let mut table = SHARED_L0_TABLE.lock();
                if let Some(entry) = table.get_mut(&l0_addr) {
                    entry.ref_count = entry.ref_count.saturating_sub(1);
                    if entry.ref_count == 0 && entry.deferred_l0.is_some() {
                        // Last shared view and owner already deferred — take frames
                        let uf = entry.deferred_user_frames.take();
                        let pf = entry.deferred_pt_frames.take();
                        let l0 = entry.deferred_l0.take();
                        table.remove(&l0_addr);
                        return (uf, pf, l0);
                    }
                    if entry.ref_count == 0 && entry.deferred_l0.is_none() {
                        table.remove(&l0_addr);
                    }
                }
                (None, None, None)
            });
            // Free deferred frames outside the lock
            if let (Some(ref uf), Some(ref pf), Some(ref l0)) = deferred {
                // #region agent log
                log::debug!("[FORK-DBG] Last shared view L0=0x{:x} freeing {} user + {} pt frames",
                    l0.addr, uf.len(), pf.len());
                // #endregion
            }
            if let (Some(uf), Some(pf), Some(l0)) = deferred {
                let rt = runtime();
                for frame in &uf { (rt.free_page)(*frame); }
                for frame in &pf { (rt.free_page)(*frame); }
                (rt.free_page)(l0);
            }
        }
        with_irqs_disabled(|| ASID_ALLOCATOR.lock().free(self.asid));
        flush_tlb_asid(self.asid);
    }
}


/// Populate an L3 page table from a 2MB block descriptor, preserving the
/// block's identity mapping as 512 individual 4KB page entries.
pub unsafe fn shatter_block_to_pages(l3_frame_addr: usize, block_entry: u64) {
    let l3_ptr = phys_to_virt(l3_frame_addr) as *mut u64;
    let block_pa = block_entry & 0x0000_FFFF_FFE0_0000; // 2MB-aligned PA
    let attrs = block_entry & 0xFFF0_0000_0000_0FFC; // upper[63:52] + lower[11:2]
    for i in 0..512u64 {
        let page_pa = block_pa + (i << 12);
        unsafe {
            l3_ptr.add(i as usize).write_volatile(page_pa | attrs | flags::VALID | flags::TABLE);
        }
    }
}

/// Map a user page at `va` to physical address `pa`.
///
/// Returns `(table_frames, installed)`:
/// - `table_frames`: any intermediate page table frames allocated during the walk.
/// - `installed`: `true` if this call installed the PTE, `false` if the PTE was
///   already valid (another thread won the race).  When `false`, the caller's
///   data frame was NOT mapped and should be freed.
pub unsafe fn map_user_page(va: usize, pa: usize, user_flags_val: u64) -> (Vec<PhysFrame>, bool) { unsafe {
    let _irq_guard = IrqGuard::new();
    let mut allocated_tables = Vec::new();
    let ttbr0: u64;
    #[cfg(target_os = "none")]
    { core::arch::asm!("mrs {}, TTBR0_EL1", out(reg) ttbr0); }
    #[cfg(not(target_os = "none"))]
    { ttbr0 = 0; }
    let l0_addr = (ttbr0 & 0x0000_FFFF_FFFF_F000) as usize;
    let l0_idx = (va >> 39) & 0x1FF;
    let l1_idx = (va >> 30) & 0x1FF;
    let l2_idx = (va >> 21) & 0x1FF;
    let l3_idx = (va >> 12) & 0x1FF;
    let l0_ptr = phys_to_virt(l0_addr) as *mut u64;
    let (l1_addr, l1_frame) = get_or_create_table_atomic(l0_ptr, l0_idx);
    if let Some(frame) = l1_frame { allocated_tables.push(frame); }
    let l1_ptr = phys_to_virt(l1_addr) as *mut u64;
    let (l2_addr, l2_frame) = get_or_create_table_atomic(l1_ptr, l1_idx);
    if let Some(frame) = l2_frame { allocated_tables.push(frame); }
    let l2_ptr = phys_to_virt(l2_addr) as *mut u64;
    let (l3_addr, l3_frame) = get_or_create_table_atomic(l2_ptr, l2_idx);
    if let Some(frame) = l3_frame { allocated_tables.push(frame); }
    let l3_ptr = phys_to_virt(l3_addr) as *mut u64;
    let pte_atomic = &*((l3_ptr.add(l3_idx)) as *const core::sync::atomic::AtomicU64);
    let existing = pte_atomic.load(core::sync::atomic::Ordering::Acquire);
    if existing & flags::VALID != 0 {
        let existing_pa = (existing & 0x0000_FFFF_FFFF_F000) as usize;
        if existing_pa != pa {
            log::debug!("[MMU] WARN: va=0x{:x} already mapped to pa=0x{:x}, wanted pa=0x{:x}",
                va, existing_pa, pa);
        }
        return (allocated_tables, false);
    }
    let entry = (pa as u64) | flags::VALID | flags::TABLE | flags::AF | flags::NG | attr_index(MAIR_NORMAL_WB) | flags::SH_INNER | user_flags_val;
    let cas_result = pte_atomic.compare_exchange(existing, entry,
        core::sync::atomic::Ordering::AcqRel, core::sync::atomic::Ordering::Acquire);
    if cas_result.is_ok() {
        #[cfg(target_os = "none")]
        { core::arch::asm!("dsb ishst", "tlbi vale1is, {va}", "dsb ish", "isb", va = in(reg) va >> 12); }
        (allocated_tables, true)
    } else {
        // CAS failed: another path installed a page between our check and CAS.
        // Return false so caller knows to free their unused page.
        (allocated_tables, false)
    }
}}

/// Same as `map_user_page` but **skips the per-page TLB invalidation**.
///
/// Use this when mapping multiple pages in a batch.  After all pages are
/// mapped, call `flush_tlb_range` (or `flush_tlb_asid`) once to flush the
/// entire range with a single DSB+ISB sequence instead of N full barriers.
///
/// The caller is responsible for issuing the TLB flush before the new
/// mappings can be safely used by userspace.
pub unsafe fn map_user_page_no_flush(va: usize, pa: usize, user_flags_val: u64) -> (Vec<PhysFrame>, bool) { unsafe {
    let _irq_guard = IrqGuard::new();
    let mut allocated_tables = Vec::new();
    let ttbr0: u64;
    #[cfg(target_os = "none")]
    { core::arch::asm!("mrs {}, TTBR0_EL1", out(reg) ttbr0); }
    #[cfg(not(target_os = "none"))]
    { ttbr0 = 0; }
    let l0_addr = (ttbr0 & 0x0000_FFFF_FFFF_F000) as usize;
    let l0_idx = (va >> 39) & 0x1FF;
    let l1_idx = (va >> 30) & 0x1FF;
    let l2_idx = (va >> 21) & 0x1FF;
    let l3_idx = (va >> 12) & 0x1FF;
    let l0_ptr = phys_to_virt(l0_addr) as *mut u64;
    let (l1_addr, l1_frame) = get_or_create_table_atomic(l0_ptr, l0_idx);
    if let Some(frame) = l1_frame { allocated_tables.push(frame); }
    let l1_ptr = phys_to_virt(l1_addr) as *mut u64;
    let (l2_addr, l2_frame) = get_or_create_table_atomic(l1_ptr, l1_idx);
    if let Some(frame) = l2_frame { allocated_tables.push(frame); }
    let l2_ptr = phys_to_virt(l2_addr) as *mut u64;
    let (l3_addr, l3_frame) = get_or_create_table_atomic(l2_ptr, l2_idx);
    if let Some(frame) = l3_frame { allocated_tables.push(frame); }
    let l3_ptr = phys_to_virt(l3_addr) as *mut u64;
    let pte_atomic = &*((l3_ptr.add(l3_idx)) as *const core::sync::atomic::AtomicU64);
    let existing = pte_atomic.load(core::sync::atomic::Ordering::Acquire);
    if existing & flags::VALID != 0 {
        let existing_pa = (existing & 0x0000_FFFF_FFFF_F000) as usize;
        if existing_pa != pa {
            log::debug!("[MMU] WARN: va=0x{:x} already mapped to pa=0x{:x}, wanted pa=0x{:x}",
                va, existing_pa, pa);
        }
        return (allocated_tables, false);
    }
    let entry = (pa as u64) | flags::VALID | flags::TABLE | flags::AF | flags::NG | attr_index(MAIR_NORMAL_WB) | flags::SH_INNER | user_flags_val;
    let cas_result = pte_atomic.compare_exchange(existing, entry,
        core::sync::atomic::Ordering::AcqRel, core::sync::atomic::Ordering::Acquire);
    if cas_result.is_ok() {
        // No TLB flush here — caller must call flush_tlb_range after mapping all pages.
        (allocated_tables, true)
    } else {
        (allocated_tables, false)
    }
}}

/// Flush TLB entries for a contiguous range of virtual addresses.
///
/// Issues `tlbi vale1is` for each page in [start_va, start_va + pages*4096),
/// then a single `dsb ish` + `isb` barrier pair.  Use after a batch of
/// `map_user_page_no_flush` calls to avoid N×(dsb+isb) overhead.
#[inline]
pub fn flush_tlb_range(start_va: usize, pages: usize) {
    #[cfg(target_os = "none")]
    unsafe {
        // Store-barrier before invalidations so PTEs are visible to other CPUs.
        core::arch::asm!("dsb ishst");
        let mut va = start_va;
        for _ in 0..pages {
            core::arch::asm!("tlbi vale1is, {}", in(reg) va >> 12);
            va += 0x1000;
        }
        // Completion barrier: wait for all invalidations, then pipeline sync.
        core::arch::asm!("dsb ish");
        core::arch::asm!("isb");
    }
    #[cfg(not(target_os = "none"))]
    let _ = (start_va, pages);
}

/// Atomically get or create a page table at `table_ptr[idx]`.
///
/// Uses compare_exchange to prevent the race where two concurrent paths
/// (e.g. mmap syscall preempted by a demand-paging fault handler) both
/// see the entry as invalid, both allocate a new table, and the second
/// write overwrites the first — orphaning all PTEs in the lost table.
unsafe fn get_or_create_table_atomic(table_ptr: *mut u64, idx: usize) -> (usize, Option<PhysFrame>) { unsafe {
    use core::sync::atomic::{AtomicU64, Ordering};
    let atomic = &*((table_ptr.add(idx)) as *const AtomicU64);

    loop {
        let entry = atomic.load(Ordering::Acquire);

        if entry & flags::VALID != 0 {
            if entry & flags::TABLE == 0 {
                // BLOCK descriptor — shatter into L3 page entries preserving the mapping
                if let Some(frame) = (runtime().alloc_page_zeroed)() {
                    shatter_block_to_pages(frame.addr, entry);
                    #[cfg(target_os = "none")]
                    core::arch::asm!("dsb ishst");
                    let new_entry = (frame.addr as u64) | flags::VALID | flags::TABLE;
                    match atomic.compare_exchange(entry, new_entry, Ordering::AcqRel, Ordering::Acquire) {
                        Ok(_) => return (frame.addr, Some(frame)),
                        Err(_) => {
                            (runtime().free_page)(frame);
                            continue;
                        }
                    }
                }
                return (0, None);
            }
            return ((entry & 0x0000_FFFF_FFFF_F000) as usize, None);
        }

        if let Some(frame) = (runtime().alloc_page_zeroed)() {
            let new_entry = (frame.addr as u64) | flags::VALID | flags::TABLE;
            match atomic.compare_exchange(entry, new_entry, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return (frame.addr, Some(frame)),
                Err(_) => {
                    (runtime().free_page)(frame);
                    continue;
                }
            }
        } else {
            return (0, None);
        }
    }
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
    
    let rt = runtime();
    let l2_table = match (rt.alloc_page)() {
        Some(frame) => frame.start_address(),
        None => return,
    };
    unsafe { core::ptr::write_bytes(l2_table as *mut u8, 0, PAGE_SIZE); }
    
    let mut l3_tables: [usize; 16] = [0; 16];
    if num_l3_blocks > 16 { return; }
    for i in 0..num_l3_blocks {
        l3_tables[i] = match (rt.alloc_page)() {
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
        #[cfg(target_os = "none")]
        core::arch::asm!("dsb ishst");
        l1_table.add(1).write_volatile((l2_table as u64) | flags::VALID | flags::TABLE);
        #[cfg(target_os = "none")]
        core::arch::asm!("dsb ish", "tlbi vmalle1", "dsb ish", "isb");
    }
}

#[cfg(target_os = "none")]
pub fn get_current_ttbr0() -> usize {
    let ttbr0: u64;
    unsafe { core::arch::asm!("mrs {}, ttbr0_el1", out(reg) ttbr0); }
    ttbr0 as usize
}

#[cfg(not(target_os = "none"))]
pub fn get_current_ttbr0() -> usize { 0 }

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

pub fn is_current_user_page_mapped(va: usize) -> bool {
    let ttbr0 = get_current_ttbr0();
    if ttbr0 == 0 { return false; }
    let l0_addr = ttbr0 & 0x0000_FFFF_FFFF_F000;
    let l0_ptr = phys_to_virt(l0_addr) as *const u64;
    is_page_mapped_ptr(l0_ptr, va & !(PAGE_SIZE - 1))
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

/// Collect (va, pa) pairs for mapped pages in [va_start, va_start + pages*PAGE_SIZE),
/// skipping empty L2 entries (2MB / 512 pages at a time).  Much faster than calling
/// `translate_user_va` per page for sparse regions (e.g. Go heap arenas).
pub fn collect_mapped_pages_sparse(
    l0_ptr: *const u64,
    va_start: usize,
    pages: usize,
) -> alloc::vec::Vec<(usize, usize)> {
    let mut result = alloc::vec::Vec::new();
    if pages == 0 { return result; }
    let va_end = match va_start.checked_add(pages.saturating_mul(PAGE_SIZE)) {
        Some(e) => e,
        None => return result,
    };

    // Walk at L2 granularity (2MB = 512 pages).
    let mut va = va_start;
    while va < va_end {
        let l0_idx = (va >> 39) & 0x1FF;
        let l1_idx = (va >> 30) & 0x1FF;
        let l2_idx = (va >> 21) & 0x1FF;

        unsafe {
            let l0_entry = l0_ptr.add(l0_idx).read_volatile();
            if l0_entry & flags::VALID == 0 {
                // Skip entire L0 region (512GB) — clamp to va_end
                let next = (va | 0x7F_FFFF_FFFF) + 1;
                va = next.min(va_end);
                continue;
            }
            let l1_ptr = phys_to_virt((l0_entry & 0x0000_FFFF_FFFF_F000) as usize) as *const u64;
            let l1_entry = l1_ptr.add(l1_idx).read_volatile();
            if l1_entry & flags::VALID == 0 {
                // Skip entire L1 region (1GB) — clamp to va_end
                let next = (va | 0x3FFF_FFFF) + 1;
                va = next.min(va_end);
                continue;
            }
            if l1_entry & flags::TABLE == 0 {
                // 1GB block mapping — unlikely for user pages, skip
                let next = (va | 0x3FFF_FFFF) + 1;
                va = next.min(va_end);
                continue;
            }
            let l2_ptr = phys_to_virt((l1_entry & 0x0000_FFFF_FFFF_F000) as usize) as *const u64;
            let l2_entry = l2_ptr.add(l2_idx).read_volatile();
            if l2_entry & flags::VALID == 0 {
                // Skip entire 2MB L2 region (512 pages)
                let next = (va | 0x1F_FFFF) + 1;
                va = next.min(va_end);
                continue;
            }
            if l2_entry & flags::TABLE == 0 {
                // 2MB block mapping — unlikely for user pages, skip
                let next = (va | 0x1F_FFFF) + 1;
                va = next.min(va_end);
                continue;
            }
            // Valid L3 table — scan pages within this 2MB range
            let l3_ptr = phys_to_virt((l2_entry & 0x0000_FFFF_FFFF_F000) as usize) as *const u64;
            let l3_start = (va >> 12) & 0x1FF;
            let l2_range_end = ((va | 0x1F_FFFF) + 1).min(va_end);
            let l3_end_idx = if l2_range_end == va_end {
                ((va_end.wrapping_sub(1) >> 12) & 0x1FF) + 1
            } else {
                512
            };
            for l3_idx in l3_start..l3_end_idx {
                let l3_entry = l3_ptr.add(l3_idx).read_volatile();
                if l3_entry & flags::VALID != 0 {
                    let page_va = (va & !0x1F_FFFF) | (l3_idx << 12);
                    let pa = ((l3_entry & 0x0000_FFFF_FFFF_F000) as usize) | (page_va & 0xFFF);
                    result.push((page_va, pa));
                }
            }
            va = l2_range_end;
        }
    }
    result
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

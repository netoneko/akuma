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
///
/// The actual MMU setup is done by the boot code before jumping to Rust.
/// This function just marks that initialization is complete and optionally
/// extends the page tables for additional RAM.
///
/// # Arguments
/// * `_ram_base` - Physical base address of RAM (unused, boot code handles this)
/// * `_ram_size` - Size of RAM in bytes (unused for now)
pub fn init(_ram_base: usize, _ram_size: usize) {
    // MMU is already enabled by boot code
    // TTBR1 has kernel mapping (0xFFFF_0000_4000_0000 -> 0x4000_0000)
    // TTBR0 has identity mapping (will be replaced per-process)
    
    MMU_INITIALIZED.store(true, Ordering::Release);
}

/// Invalidate all TLB entries
pub fn flush_tlb_all() {
    unsafe {
        core::arch::asm!(
            "dsb ishst",
            "tlbi vmalle1",
            "dsb ish",
            "isb"
        );
    }
}

/// Invalidate TLB entries for a specific ASID
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

/// Invalidate TLB entry for a specific virtual address
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

use alloc::boxed::Box;
use alloc::vec::Vec;
use spinning_top::Spinlock;

use crate::pmm::{self, PhysFrame};

/// Maximum ASID value (16-bit on most AArch64 implementations)
const MAX_ASID: u16 = 256; // Use 8-bit ASID for compatibility

/// ASID allocator
static ASID_ALLOCATOR: Spinlock<AsidAllocator> = Spinlock::new(AsidAllocator::new());

struct AsidAllocator {
    next_asid: u16,
    // Simple bitmap for tracking used ASIDs
    used: [u64; 4], // 256 bits
}

impl AsidAllocator {
    const fn new() -> Self {
        Self {
            next_asid: 1, // ASID 0 is reserved for kernel
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
                // Found a free ASID
                self.used[word] |= 1 << bit;
                self.next_asid = if asid + 1 >= MAX_ASID { 1 } else { asid + 1 };
                return Some(asid);
            }

            asid = if asid + 1 >= MAX_ASID { 1 } else { asid + 1 };
            if asid == start {
                // Wrapped around, no free ASIDs
                return None;
            }
        }
    }

    fn free(&mut self, asid: u16) {
        if asid > 0 && asid < MAX_ASID {
            let word = (asid / 64) as usize;
            let bit = asid % 64;
            if word < self.used.len() {
                self.used[word] &= !(1 << bit);
            }
        }
    }
}

/// User address space with its own page tables
pub struct UserAddressSpace {
    /// L0 page table (physical address)
    l0_frame: PhysFrame,
    /// Allocated page table frames (for cleanup)
    page_table_frames: Vec<PhysFrame>,
    /// Allocated user pages (for cleanup)
    user_frames: Vec<PhysFrame>,
    /// ASID for this address space
    asid: u16,
}

impl UserAddressSpace {
    /// Create a new empty user address space
    pub fn new() -> Option<Self> {
        // Allocate L0 page table
        let l0_frame = pmm::alloc_page_zeroed()?;

        // Allocate an ASID
        let asid = ASID_ALLOCATOR.lock().alloc()?;

        Some(Self {
            l0_frame,
            page_table_frames: Vec::new(),
            user_frames: Vec::new(),
            asid,
        })
    }

    /// Get the TTBR0 value for this address space
    pub fn ttbr0(&self) -> u64 {
        // TTBR0_EL1 format: ASID in bits [63:48], BADDR in bits [47:1]
        ((self.asid as u64) << 48) | (self.l0_frame.addr as u64)
    }

    /// Get the ASID
    pub fn asid(&self) -> u16 {
        self.asid
    }

    /// Map a virtual address to a physical frame with given flags
    ///
    /// # Arguments
    /// * `va` - Virtual address (must be page-aligned)
    /// * `pa` - Physical address (must be page-aligned)
    /// * `user_flags` - Page flags for user access
    pub fn map_page(&mut self, va: usize, pa: usize, user_flags: u64) -> Result<(), &'static str> {
        if va & (PAGE_SIZE - 1) != 0 || pa & (PAGE_SIZE - 1) != 0 {
            return Err("Addresses must be page-aligned");
        }

        // Extract page table indices from virtual address
        // VA format for 4KB granule, 48-bit:
        // [47:39] L0 index, [38:30] L1 index, [29:21] L2 index, [20:12] L3 index, [11:0] offset
        let l0_idx = (va >> 39) & 0x1FF;
        let l1_idx = (va >> 30) & 0x1FF;
        let l2_idx = (va >> 21) & 0x1FF;
        let l3_idx = (va >> 12) & 0x1FF;

        // Walk/create page tables
        let l0_ptr = self.l0_frame.addr as *mut u64;
        let l1_frame = self.get_or_create_table(l0_ptr, l0_idx)?;
        let l1_ptr = l1_frame.addr as *mut u64;

        let l2_frame = self.get_or_create_table(l1_ptr, l1_idx)?;
        let l2_ptr = l2_frame.addr as *mut u64;

        let l3_frame = self.get_or_create_table(l2_ptr, l2_idx)?;
        let l3_ptr = l3_frame.addr as *mut u64;

        // Create L3 entry (4KB page descriptor)
        let entry = (pa as u64)
            | flags::VALID
            | flags::TABLE // For L3, bit 1 must be 1 (page descriptor)
            | flags::AF
            | flags::NG // Non-global (uses ASID)
            | attr_index(MAIR_NORMAL_WB)
            | flags::SH_INNER
            | user_flags;

        unsafe {
            l3_ptr.add(l3_idx).write_volatile(entry);
        }

        Ok(())
    }

    /// Get or create a page table entry, returning the next level table
    fn get_or_create_table(
        &mut self,
        table_ptr: *mut u64,
        idx: usize,
    ) -> Result<PhysFrame, &'static str> {
        unsafe {
            let entry = table_ptr.add(idx).read_volatile();

            if entry & flags::VALID != 0 {
                // Entry already exists, extract address
                let addr = (entry & 0x0000_FFFF_FFFF_F000) as usize;
                Ok(PhysFrame::new(addr))
            } else {
                // Need to allocate a new page table
                let frame = pmm::alloc_page_zeroed().ok_or("Out of memory for page table")?;
                self.page_table_frames.push(frame);

                // Create table descriptor
                let new_entry = (frame.addr as u64) | flags::VALID | flags::TABLE;
                table_ptr.add(idx).write_volatile(new_entry);

                Ok(frame)
            }
        }
    }

    /// Map a range of pages for user code/data
    pub fn map_range(
        &mut self,
        va_start: usize,
        pa_start: usize,
        size: usize,
        user_flags: u64,
    ) -> Result<(), &'static str> {
        let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
        for i in 0..pages {
            let va = va_start + i * PAGE_SIZE;
            let pa = pa_start + i * PAGE_SIZE;
            self.map_page(va, pa, user_flags)?;
        }
        Ok(())
    }

    /// Allocate and map a new page at the given virtual address
    pub fn alloc_and_map(&mut self, va: usize, user_flags: u64) -> Result<PhysFrame, &'static str> {
        let frame = pmm::alloc_page_zeroed().ok_or("Out of memory for user page")?;
        self.user_frames.push(frame);
        self.map_page(va, frame.addr, user_flags)?;
        Ok(frame)
    }

    /// Unmap a page (doesn't free the physical frame)
    pub fn unmap_page(&mut self, va: usize) -> Result<(), &'static str> {
        let l0_idx = (va >> 39) & 0x1FF;
        let l1_idx = (va >> 30) & 0x1FF;
        let l2_idx = (va >> 21) & 0x1FF;
        let l3_idx = (va >> 12) & 0x1FF;

        unsafe {
            let l0_ptr = self.l0_frame.addr as *mut u64;
            let l0_entry = l0_ptr.add(l0_idx).read_volatile();
            if l0_entry & flags::VALID == 0 {
                return Ok(()); // Not mapped
            }

            let l1_ptr = (l0_entry & 0x0000_FFFF_FFFF_F000) as *mut u64;
            let l1_entry = l1_ptr.add(l1_idx).read_volatile();
            if l1_entry & flags::VALID == 0 {
                return Ok(());
            }

            let l2_ptr = (l1_entry & 0x0000_FFFF_FFFF_F000) as *mut u64;
            let l2_entry = l2_ptr.add(l2_idx).read_volatile();
            if l2_entry & flags::VALID == 0 {
                return Ok(());
            }

            let l3_ptr = (l2_entry & 0x0000_FFFF_FFFF_F000) as *mut u64;
            l3_ptr.add(l3_idx).write_volatile(0);
        }

        flush_tlb_page(va);
        Ok(())
    }

    /// Activate this address space (set TTBR0_EL1)
    ///
    /// The kernel runs in TTBR1 (upper half), so we can safely switch TTBR0
    /// to this user address space.
    pub fn activate(&self) {
        let ttbr0 = self.ttbr0();
        unsafe {
            core::arch::asm!(
                "msr ttbr0_el1, {ttbr0}",
                "isb",
                ttbr0 = in(reg) ttbr0
            );
        }
    }

    /// Deactivate user address space (clear TTBR0_EL1)
    ///
    /// Sets TTBR0 to an empty/invalid state. Any user-space access will fault.
    pub fn deactivate() {
        unsafe {
            // Set TTBR0 to 0 - any user access will fault
            core::arch::asm!(
                "msr ttbr0_el1, xzr",
                "isb"
            );
        }
        // Flush TLB for TTBR0
        flush_tlb_all();
    }
}

impl Drop for UserAddressSpace {
    fn drop(&mut self) {
        // Free all user pages
        for frame in &self.user_frames {
            pmm::free_page(*frame);
        }

        // Free all page table frames
        for frame in &self.page_table_frames {
            pmm::free_page(*frame);
        }

        // Free L0 table
        pmm::free_page(self.l0_frame);

        // Free ASID
        ASID_ALLOCATOR.lock().free(self.asid);

        // Flush TLB for this ASID
        flush_tlb_asid(self.asid);
    }
}

/// User page flags
pub mod user_flags {
    use super::flags;

    /// Read-only user page
    pub const RO: u64 = flags::AP_RO_ALL;

    /// Read-write user page
    pub const RW: u64 = flags::AP_RW_ALL;

    /// Executable user page (no UXN)
    pub const EXEC: u64 = flags::AP_RO_ALL;

    /// Read-write, non-executable user page
    pub const RW_NO_EXEC: u64 = flags::AP_RW_ALL | flags::UXN | flags::PXN;

    /// Read-only, executable user page (code)
    pub const RX: u64 = flags::AP_RO_ALL | flags::PXN;
}


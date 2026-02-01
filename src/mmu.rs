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

// =============================================================================
// Physical/Virtual Address Translation
// =============================================================================
//
// These functions provide explicit translation between physical and virtual
// addresses. Currently the kernel uses identity mapping (VA == PA), but having
// these functions:
// 1. Makes the code explicit about which addresses are physical vs virtual
// 2. Allows future migration to non-identity-mapped kernel
// 3. Ensures correct behavior regardless of active TTBR0 page tables

/// Convert a physical address to a kernel-accessible virtual address
///
/// For identity-mapped memory regions, this returns the same address.
/// This should be used whenever dereferencing a physical address returned
/// by PMM or stored in page table entries.
#[inline(always)]
pub fn phys_to_virt(paddr: usize) -> *mut u8 {
    // Identity mapping: VA == PA for kernel memory (0x40000000+)
    // The kernel has this mapped as 1GB blocks at L1[1] and L1[2]
    paddr as *mut u8
}

/// Convert a kernel virtual address to a physical address
///
/// For identity-mapped memory regions, this returns the same address.
/// This should be used when passing addresses to hardware (DMA, VirtIO)
/// or when storing addresses in page table entries.
#[inline(always)]
pub fn virt_to_phys(vaddr: usize) -> usize {
    // Identity mapping: PA == VA for kernel memory
    vaddr
}

/// Invalidate all TLB entries
pub fn flush_tlb_all() {
    unsafe {
        core::arch::asm!("dsb ishst", "tlbi vmalle1", "dsb ish", "isb");
    }
}

/// Get the boot TTBR0 value (stored by boot code)
///
/// This returns the original kernel page table address, NOT the current TTBR0.
/// Use this when spawning new threads to ensure they get the kernel's page tables,
/// not a user process's page tables that might be active.
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
    /// Create a new user address space with kernel identity mapping
    ///
    /// The kernel runs at physical addresses (0x40000000), so we need to include
    /// the kernel's identity mapping in every user address space. This allows the
    /// kernel to continue executing when TTBR0 is switched to this address space.
    pub fn new() -> Option<Self> {
        // Allocate L0 page table
        let l0_frame = pmm::alloc_page_zeroed()?;
        // Track L0 frame (PID=0 since we don't have the process yet)
        pmm::track_frame(l0_frame, pmm::FrameSource::UserPageTable, 0);

        // Allocate an ASID (with IRQ protection to prevent deadlock)
        let asid = crate::irq::with_irqs_disabled(|| ASID_ALLOCATOR.lock().alloc())?;

        let mut addr_space = Self {
            l0_frame,
            page_table_frames: Vec::new(),
            user_frames: Vec::new(),
            asid,
        };

        // Add kernel identity mapping (device + RAM)
        // This mirrors the boot page tables so kernel can run while TTBR0 is active
        addr_space.add_kernel_mappings().ok()?;

        Some(addr_space)
    }

    /// Add kernel identity mappings to this address space
    ///
    /// Maps kernel RAM (0x40000000+) and device memory as 1GB blocks.
    /// The first 1GB (0x00000000-0x3FFFFFFF) is left for user mappings at 4KB granularity.
    fn add_kernel_mappings(&mut self) -> Result<(), &'static str> {
        // We need L1 table entries for identity mapping
        // L0[0] -> L1 table
        // L1[0] = 0x00000000-0x3FFFFFFF (user code space - NOT mapped as block)
        // L1[1] = 0x40000000-0x7FFFFFFF (kernel RAM - 1GB block)
        // L1[2] = 0x80000000-0xBFFFFFFF (more RAM - 1GB block)

        // Allocate L1 table for low 512GB region
        let l1_frame =
            pmm::alloc_page_zeroed().ok_or("Failed to allocate L1 table for kernel mapping")?;
        pmm::track_frame(l1_frame, pmm::FrameSource::UserPageTable, 0);
        self.page_table_frames.push(l1_frame);

        // Set L0[0] to point to L1
        // Use phys_to_virt to get a kernel VA for the physical page table address
        let l0_ptr = phys_to_virt(self.l0_frame.addr) as *mut u64;
        unsafe {
            let l1_entry = (l1_frame.addr as u64) | flags::VALID | flags::TABLE;
            core::ptr::write_volatile(l0_ptr, l1_entry);
        }

        // Set up L1 entries
        let l1_ptr = phys_to_virt(l1_frame.addr) as *mut u64;

        // L1[0]: Leave unmapped (or map for device access at 2MB granularity later)
        // User code at 0x400000 will be mapped with 4KB pages via map_page()
        // Device memory (GIC, UART at 0x08-0x09 million) needs separate handling

        // For now, set up L2 table for first 1GB to allow both device access and user code
        let l2_frame = pmm::alloc_page_zeroed().ok_or("Failed to allocate L2 table")?;
        pmm::track_frame(l2_frame, pmm::FrameSource::UserPageTable, 0);
        self.page_table_frames.push(l2_frame);

        unsafe {
            // L1[0] -> L2 table (for fine-grained mapping of first 1GB)
            let l2_entry = (l2_frame.addr as u64) | flags::VALID | flags::TABLE;
            core::ptr::write_volatile(l1_ptr.add(0), l2_entry);
        }

        // Map device memory regions as 2MB blocks in L2
        // Each L2 entry covers 2MB (0x200000)
        // GIC at 0x08000000 = L2 index 64
        // UART at 0x09000000 = L2 index 72
        // VirtIO MMIO at 0x0a000000 = L2 index 80
        let l2_ptr = phys_to_virt(l2_frame.addr) as *mut u64;
        let device_block_flags = flags::VALID
            | flags::BLOCK
            | flags::AF
            | attr_index(MAIR_DEVICE_NGNRNE)
            | flags::PXN
            | flags::UXN
            | flags::SH_OUTER;

        // Map 0x08000000-0x0BFFFFFF (GIC, UART, VirtIO MMIO) as device memory (2MB blocks)
        // Extended to index 96 to cover VirtIO MMIO region at 0x0a000000-0x0a000e00
        for i in 64..96 {
            // Covers 0x08000000 - 0x0BFFFFFF (64MB)
            let pa = (i as u64) * 0x200000; // 2MB * index
            unsafe {
                core::ptr::write_volatile(l2_ptr.add(i), pa | device_block_flags);
            }
        }

        // L1[1]: Kernel RAM 0x40000000-0x7FFFFFFF (1GB block)
        // Flags: valid, block, AF, normal memory, kernel-only access
        let kernel_ram_flags = flags::VALID | flags::BLOCK | flags::AF
            | attr_index(MAIR_NORMAL_WB)
            | flags::UXN  // No user execute
            | flags::SH_INNER
            | (0b00 << 6); // AP[2:1] = 00 = RW at EL1, no access at EL0
        unsafe {
            core::ptr::write_volatile(l1_ptr.add(1), 0x4000_0000u64 | kernel_ram_flags);
        }

        // L1[2]: More RAM 0x80000000-0xBFFFFFFF (1GB block)
        unsafe {
            core::ptr::write_volatile(l1_ptr.add(2), 0x8000_0000u64 | kernel_ram_flags);
        }

        Ok(())
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

        // Walk/create page tables (use phys_to_virt for all PA->pointer conversions)
        let l0_ptr = phys_to_virt(self.l0_frame.addr) as *mut u64;
        let l1_frame = self.get_or_create_table(l0_ptr, l0_idx)?;
        let l1_ptr = phys_to_virt(l1_frame.addr) as *mut u64;

        let l2_frame = self.get_or_create_table(l1_ptr, l1_idx)?;
        let l2_ptr = phys_to_virt(l2_frame.addr) as *mut u64;

        let l3_frame = self.get_or_create_table(l2_ptr, l2_idx)?;
        let l3_ptr = phys_to_virt(l3_frame.addr) as *mut u64;

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
                // Track as page table frame (PID=0 since we don't have process context here)
                pmm::track_frame(frame, pmm::FrameSource::UserPageTable, 0);
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
        // Track as ELF loader allocation (PID=0 since we're loading before process is created)
        pmm::track_frame(frame, pmm::FrameSource::ElfLoader, 0);
        self.user_frames.push(frame);
        self.map_page(va, frame.addr, user_flags)?;
        Ok(frame)
    }

    /// Track a frame that was allocated externally (e.g., by sys_mmap)
    ///
    /// This adds the frame to user_frames so it will be freed when the
    /// address space is dropped.
    pub fn track_user_frame(&mut self, frame: PhysFrame) {
        self.user_frames.push(frame);
    }

    /// Remove a frame from tracking (e.g., when munmap frees it early)
    ///
    /// This prevents double-free when the address space is dropped.
    /// Does nothing if the frame isn't found (already removed or never tracked).
    pub fn remove_user_frame(&mut self, frame: PhysFrame) {
        if let Some(idx) = self.user_frames.iter().position(|f| f.addr == frame.addr) {
            self.user_frames.swap_remove(idx);
        }
    }

    /// Unmap a page (doesn't free the physical frame)
    pub fn unmap_page(&mut self, va: usize) -> Result<(), &'static str> {
        let l0_idx = (va >> 39) & 0x1FF;
        let l1_idx = (va >> 30) & 0x1FF;
        let l2_idx = (va >> 21) & 0x1FF;
        let l3_idx = (va >> 12) & 0x1FF;

        unsafe {
            let l0_ptr = phys_to_virt(self.l0_frame.addr) as *mut u64;
            let l0_entry = l0_ptr.add(l0_idx).read_volatile();
            if l0_entry & flags::VALID == 0 {
                return Ok(()); // Not mapped
            }

            let l1_addr = (l0_entry & 0x0000_FFFF_FFFF_F000) as usize;
            let l1_ptr = phys_to_virt(l1_addr) as *mut u64;
            let l1_entry = l1_ptr.add(l1_idx).read_volatile();
            if l1_entry & flags::VALID == 0 {
                return Ok(());
            }

            let l2_addr = (l1_entry & 0x0000_FFFF_FFFF_F000) as usize;
            let l2_ptr = phys_to_virt(l2_addr) as *mut u64;
            let l2_entry = l2_ptr.add(l2_idx).read_volatile();
            if l2_entry & flags::VALID == 0 {
                return Ok(());
            }

            let l3_addr = (l2_entry & 0x0000_FFFF_FFFF_F000) as usize;
            let l3_ptr = phys_to_virt(l3_addr) as *mut u64;
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
        
        // CRITICAL: Proper TTBR0 switch sequence:
        // 1. Flush TLB BEFORE switch - remove stale entries from old address space
        // 2. DSB to complete pending operations
        // 3. Switch TTBR0
        // 4. ISB to ensure instruction stream sees new page tables
        // 5. Flush TLB AFTER switch - ensure no speculative entries from old TTBR0
        //
        // The double flush is paranoid but avoids any window where stale entries
        // could cause issues.
        
        flush_tlb_all();  // Pre-switch flush
        
        unsafe {
            core::arch::asm!(
                // Ensure TLB flush and all previous memory accesses complete
                "dsb ish",
                // Switch TTBR0 to user address space
                "msr ttbr0_el1, {ttbr0}",
                // Synchronization barrier for the switch
                "isb",
                ttbr0 = in(reg) ttbr0
            );
        }
        
        flush_tlb_all();  // Post-switch flush
    }

    /// Deactivate user address space (restore boot page tables to TTBR0)
    ///
    /// Restores the boot page tables so the kernel identity mapping is active.
    pub fn deactivate() {
        // Get the boot TTBR0 value (stored by boot code)
        let boot_ttbr0: u64 = unsafe {
            let addr: u64;
            core::arch::asm!(
                "adrp {tmp}, boot_ttbr0_addr",
                "add {tmp}, {tmp}, :lo12:boot_ttbr0_addr",
                "ldr {out}, [{tmp}]",
                tmp = out(reg) _,
                out = out(reg) addr,
            );
            addr
        };

        // CRITICAL: Proper TTBR0 switch sequence (same as activate)
        // 1. Pre-switch flush - remove stale user space entries
        // 2. DSB to complete pending operations  
        // 3. Switch TTBR0 back to boot page tables
        // 4. ISB to ensure instruction stream sees new page tables
        // 5. Post-switch flush - ensure clean state
        
        flush_tlb_all();  // Pre-switch flush
        
        unsafe {
            // Restore boot page tables
            core::arch::asm!(
                // Ensure TLB flush and all previous memory accesses complete
                "dsb ish",
                // Switch TTBR0 back to boot page tables
                "msr ttbr0_el1, {ttbr0}",
                // Synchronization barrier for the switch
                "isb",
                ttbr0 = in(reg) boot_ttbr0
            );
        }
        
        flush_tlb_all();  // Post-switch flush
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

        // Free ASID (with IRQ protection to prevent deadlock)
        crate::irq::with_irqs_disabled(|| ASID_ALLOCATOR.lock().free(self.asid));

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

/// Map a user page in the current TTBR0 address space
///
/// This is used by sys_mmap to add pages to the running process.
/// Returns a Vec of newly allocated page table frames that the caller should track
/// for cleanup when the process exits.
///
/// SAFETY: Caller must ensure VA and PA are page-aligned and valid.
pub unsafe fn map_user_page(va: usize, pa: usize, user_flags_val: u64) -> Vec<PhysFrame> { unsafe {
    let mut allocated_tables = Vec::new();

    // Get current TTBR0
    let ttbr0: u64;
    core::arch::asm!("mrs {}, TTBR0_EL1", out(reg) ttbr0);

    // Extract L0 table address (bits 47:1, assuming 4KB granule)
    let l0_addr = (ttbr0 & 0x0000_FFFF_FFFF_F000) as usize;

    // Extract page table indices from virtual address
    let l0_idx = (va >> 39) & 0x1FF;
    let l1_idx = (va >> 30) & 0x1FF;
    let l2_idx = (va >> 21) & 0x1FF;
    let l3_idx = (va >> 12) & 0x1FF;

    // Walk the page tables, creating entries as needed
    // All PA->pointer conversions go through phys_to_virt
    let l0_ptr = phys_to_virt(l0_addr) as *mut u64;
    let (l1_addr, l1_frame) = get_or_create_table(l0_ptr, l0_idx);
    if let Some(frame) = l1_frame {
        allocated_tables.push(frame);
    }
    let l1_ptr = phys_to_virt(l1_addr) as *mut u64;

    let (l2_addr, l2_frame) = get_or_create_table(l1_ptr, l1_idx);
    if let Some(frame) = l2_frame {
        allocated_tables.push(frame);
    }
    let l2_ptr = phys_to_virt(l2_addr) as *mut u64;

    let (l3_addr, l3_frame) = get_or_create_table(l2_ptr, l2_idx);
    if let Some(frame) = l3_frame {
        allocated_tables.push(frame);
    }
    let l3_ptr = phys_to_virt(l3_addr) as *mut u64;

    // Create L3 entry (4KB page descriptor)
    let entry = (pa as u64)
        | flags::VALID
        | flags::TABLE // For L3, bit 1 must be 1 (page descriptor)
        | flags::AF
        | flags::NG // Non-global (uses ASID)
        | attr_index(MAIR_NORMAL_WB)
        | flags::SH_INNER
        | user_flags_val;

    l3_ptr.add(l3_idx).write_volatile(entry);

    // Flush TLB for this VA
    core::arch::asm!(
        "dsb ishst",
        "tlbi vale1is, {va}",
        "dsb ish",
        "isb",
        va = in(reg) va >> 12,
    );

    allocated_tables
}}

/// Get or create a page table entry, returning the next level table physical address
/// and optionally the newly allocated frame (if one was created).
///
/// Note: The returned address is a PHYSICAL address. Callers must use phys_to_virt()
/// before dereferencing it.
///
/// Returns: (physical_address, Option<PhysFrame>) where the frame is Some if a new
/// page table was allocated (caller should track it for cleanup).
unsafe fn get_or_create_table(table_ptr: *mut u64, idx: usize) -> (usize, Option<PhysFrame>) { unsafe {
    let entry = table_ptr.add(idx).read_volatile();

    if entry & flags::VALID != 0 {
        // Entry exists, extract physical address
        ((entry & 0x0000_FFFF_FFFF_F000) as usize, None)
    } else {
        // Need to allocate a new page table
        if let Some(frame) = crate::pmm::alloc_page_zeroed() {
            let new_entry = (frame.addr as u64) | flags::VALID | flags::TABLE;
            table_ptr.add(idx).write_volatile(new_entry);
            (frame.addr, Some(frame)) // Return physical address and the frame
        } else {
            // Allocation failed - this is bad, but return 0
            (0, None)
        }
    }
}}

// ============================================================================
// Kernel Memory Protection
// ============================================================================

unsafe extern "C" {
    static _text_start: u8;
    static _text_end: u8;
    static _rodata_start: u8;
    static _rodata_end: u8;
    static _data_start: u8;
    static _data_end: u8;
    static _kernel_phys_end: u8;
}

/// Protect kernel code by marking it read-only
///
/// This function uses 4KB page granularity to precisely protect only the
/// .text and .rodata sections, leaving .data/.bss writable.
///
/// The function dynamically determines which 2MB blocks need fine-grained
/// (4KB) protection based on where .text/.rodata/.data actually reside.
pub fn protect_kernel_code() {
    let text_start = unsafe { &_text_start as *const u8 as usize };
    let text_end = unsafe { &_text_end as *const u8 as usize };
    let rodata_start = unsafe { &_rodata_start as *const u8 as usize };
    let rodata_end = unsafe { &_rodata_end as *const u8 as usize };
    let data_start = unsafe { &_data_start as *const u8 as usize };
    let kernel_end = unsafe { &_kernel_phys_end as *const u8 as usize };

    crate::safe_print!(384, 
        "[MMU] Kernel sections:\n  .text:   0x{:08x}-0x{:08x} ({} KB)\n  .rodata: 0x{:08x}-0x{:08x} ({} KB)\n  .data:   0x{:08x}-0x{:08x} ({} KB)\n",
        text_start,
        text_end,
        (text_end - text_start) / 1024,
        rodata_start,
        rodata_end,
        (rodata_end - rodata_start) / 1024,
        data_start,
        kernel_end,
        (kernel_end - data_start) / 1024,
    );

    const BLOCK_SIZE_2MB: usize = 2 * 1024 * 1024;
    const RAM_BASE: usize = 0x40000000;
    
    // Calculate which 2MB blocks contain code that needs protection
    // We need L3 tables for any 2MB block that contains BOTH:
    // - Read-only sections (.text or .rodata)
    // - Read-write sections (.data or beyond)
    // If a block contains only RO or only RW, we can use a 2MB block descriptor
    
    // Find the 2MB block indices for section boundaries
    let text_block_start = (text_start - RAM_BASE) / BLOCK_SIZE_2MB;
    let rodata_block_end = (rodata_end - RAM_BASE + BLOCK_SIZE_2MB - 1) / BLOCK_SIZE_2MB;
    let data_block_start = (data_start - RAM_BASE) / BLOCK_SIZE_2MB;
    
    // Blocks that need L3 tables: those containing both RO and RW data
    // This is the range from where RO starts to where RW starts (inclusive)
    let l3_block_start = text_block_start;
    let l3_block_end = if data_block_start > rodata_block_end {
        rodata_block_end  // .data starts in a later block, only need L3 up to rodata
    } else {
        data_block_start + 1  // .data starts in same or earlier block as rodata ends
    };
    let num_l3_blocks = l3_block_end - l3_block_start;
    
    crate::safe_print!(128, "[MMU] Need L3 tables for blocks {}-{} ({} blocks)\n", 
        l3_block_start, l3_block_end - 1, num_l3_blocks);
    
    // Allocate L2 table for the 0x40000000-0x7FFFFFFF region
    let l2_table = match crate::pmm::alloc_page() {
        Some(frame) => frame.start_address(),
        None => {
            crate::console::print("[MMU] ERROR: Cannot allocate L2 table\n");
            return;
        }
    };
    unsafe { core::ptr::write_bytes(l2_table as *mut u8, 0, PAGE_SIZE); }
    
    // Allocate L3 tables for blocks that need fine-grained protection
    let mut l3_tables: [usize; 16] = [0; 16];  // Support up to 16 L3 tables (32MB of kernel)
    if num_l3_blocks > 16 {
        crate::console::print("[MMU] ERROR: Kernel too large for protection\n");
        return;
    }
    
    for i in 0..num_l3_blocks {
        let l3_table = match crate::pmm::alloc_page() {
            Some(frame) => frame.start_address(),
            None => {
                crate::console::print("[MMU] ERROR: Cannot allocate L3 table\n");
                return;
            }
        };
        unsafe { core::ptr::write_bytes(l3_table as *mut u8, 0, PAGE_SIZE); }
        l3_tables[i] = l3_table;
    }
    
    let l2_ptr = l2_table as *mut u64;
    
    // L2 block descriptor flags (2MB blocks)
    const BLOCK_RW: u64 = flags::VALID | (3 << 2) | flags::SH_INNER | flags::AF;
    
    // L3 page descriptor flags (4KB pages)
    const PAGE_RW: u64 = flags::VALID | flags::TABLE | (3 << 2) | flags::SH_INNER | flags::AF;
    const PAGE_RO: u64 = flags::VALID | flags::TABLE | (3 << 2) | flags::SH_INNER | flags::AF | flags::AP_RO_EL1;
    
    // Fill L2 table
    for i in 0..512 {
        let block_addr = RAM_BASE + i * BLOCK_SIZE_2MB;
        
        if i >= l3_block_start && i < l3_block_end {
            // This block needs L3 table for fine-grained protection
            let l3_idx = i - l3_block_start;
            let l3_table = l3_tables[l3_idx];
            let l3_ptr = l3_table as *mut u64;
            
            // Fill L3 table with 4KB page mappings
            for j in 0..512 {
                let page_addr = block_addr + j * PAGE_SIZE;
                
                let in_text = page_addr >= text_start && page_addr < text_end;
                let in_rodata = page_addr >= rodata_start && page_addr < rodata_end;
                let is_read_only = in_text || in_rodata;
                
                let entry = if is_read_only {
                    (page_addr as u64) | PAGE_RO
                } else {
                    (page_addr as u64) | PAGE_RW
                };
                
                unsafe { l3_ptr.add(j).write_volatile(entry); }
            }
            
            // L2 entry points to L3 table
            let l3_table_entry = (l3_table as u64) | flags::VALID | flags::TABLE;
            unsafe { l2_ptr.add(i).write_volatile(l3_table_entry); }
        } else {
            // This block can use 2MB block descriptor (all RW)
            let entry = (block_addr as u64) | BLOCK_RW;
            unsafe { l2_ptr.add(i).write_volatile(entry); }
        }
    }
    
    // Update L1[1] to point to our L2 table
    let l2_table_entry = (l2_table as u64) | flags::VALID | flags::TABLE;
    let l0_table = get_boot_ttbr0() as *mut u64;
    
    unsafe {
        let l0_entry = l0_table.add(0).read_volatile();
        let l1_table = (l0_entry & 0x0000_FFFF_FFFF_F000) as *mut u64;
        
        crate::safe_print!(96, "[MMU] L0={:#x}, L1={:#x}, L2={:#x}\n", 
            l0_table as u64, l1_table as u64, l2_table as u64);
        
        core::arch::asm!("dsb ishst");
        l1_table.add(1).write_volatile(l2_table_entry);
        core::arch::asm!("dsb ish", "tlbi vmalle1", "dsb ish", "isb");
    }
    
    // Count protected pages
    let text_pages = (text_end - text_start + PAGE_SIZE - 1) / PAGE_SIZE;
    let rodata_pages = (rodata_end - rodata_start + PAGE_SIZE - 1) / PAGE_SIZE;
    crate::safe_print!(128, "[MMU] Protected {} text + {} rodata pages (4KB each)\n", 
        text_pages, rodata_pages);
    
    crate::console::print("[MMU] Kernel code protection ENABLED\n");
}

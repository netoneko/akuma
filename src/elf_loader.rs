//! ELF Loader
//!
//! Parses and loads ELF binaries into user address space.
//! Uses the `elf` crate for parsing.

use elf::ElfBytes;
use elf::abi::{EM_AARCH64, ET_EXEC, PF_R, PF_W, PF_X, PT_LOAD};
use elf::endian::LittleEndian;

use crate::mmu::{PAGE_SIZE, UserAddressSpace, user_flags};

/// Enable debug output for ELF loading
/// Set to false to reduce boot verbosity
pub const DEBUG_ELF_LOADING: bool = true;

/// Result of loading an ELF binary
pub struct LoadedElf {
    /// Entry point virtual address
    pub entry_point: usize,
    /// User address space with mapped pages
    pub address_space: UserAddressSpace,
    /// Highest mapped address (for stack placement)
    pub brk: usize,
}

/// Error during ELF loading
#[derive(Debug)]
pub enum ElfError {
    /// Invalid ELF format
    InvalidFormat(&'static str),
    /// Wrong architecture (not AArch64)
    WrongArchitecture,
    /// Not an executable
    NotExecutable,
    /// Out of memory
    OutOfMemory,
    /// Address space creation failed
    AddressSpaceFailed,
    /// Mapping failed
    MappingFailed(&'static str),
}

impl core::fmt::Display for ElfError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ElfError::InvalidFormat(msg) => write!(f, "Invalid ELF format: {}", msg),
            ElfError::WrongArchitecture => write!(f, "Not an AArch64 binary"),
            ElfError::NotExecutable => write!(f, "Not an executable"),
            ElfError::OutOfMemory => write!(f, "Out of memory"),
            ElfError::AddressSpaceFailed => write!(f, "Failed to create address space"),
            ElfError::MappingFailed(msg) => write!(f, "Mapping failed: {}", msg),
        }
    }
}

use alloc::collections::BTreeMap;

const R_AARCH64_RELATIVE: u32 = 1027;

/// Load an ELF binary from memory
///
/// # Arguments
/// * `elf_data` - Raw ELF file data
///
/// # Returns
/// LoadedElf with entry point and configured address space
pub fn load_elf(elf_data: &[u8]) -> Result<LoadedElf, ElfError> {
    // Parse ELF header
    let elf = ElfBytes::<LittleEndian>::minimal_parse(elf_data)
        .map_err(|_| ElfError::InvalidFormat("Parse failed"))?;

    // Verify architecture
    if elf.ehdr.e_machine != EM_AARCH64 {
        return Err(ElfError::WrongArchitecture);
    }

    // Verify it's an executable (not shared lib or relocatable)
    if elf.ehdr.e_type != ET_EXEC {
        return Err(ElfError::NotExecutable);
    }

    // Get entry point
    let entry_point = elf.ehdr.e_entry as usize;

    // Create user address space
    let mut address_space = UserAddressSpace::new().ok_or(ElfError::AddressSpaceFailed)?;

    // Track highest address for brk
    let mut brk: usize = 0;

    // Track already-mapped pages (VA -> PA) to avoid double allocation
    let mut mapped_pages: BTreeMap<usize, usize> = BTreeMap::new();

    // Get program headers
    let segments = elf
        .segments()
        .ok_or(ElfError::InvalidFormat("No program headers"))?;

    // Load each PT_LOAD segment
    for phdr in segments.iter() {
        if phdr.p_type != PT_LOAD {
            continue;
        }

        let vaddr = phdr.p_vaddr as usize;
        let memsz = phdr.p_memsz as usize;
        let filesz = phdr.p_filesz as usize;
        let offset = phdr.p_offset as usize;
        let flags = phdr.p_flags;

        if DEBUG_ELF_LOADING {
            crate::safe_print!(128, "[ELF] Segment: VA=0x{:08x} filesz=0x{:x} memsz=0x{:x} flags={}{}{}\n",
                vaddr, filesz, memsz,
                if flags & PF_R != 0 { "R" } else { "-" },
                if flags & PF_W != 0 { "W" } else { "-" },
                if flags & PF_X != 0 { "X" } else { "-" });
        }

        // Use appropriate flags based on segment permissions
        // Note: If segment is writable, use RW_NO_EXEC to handle BSS overlaps
        let page_flags = if (flags & PF_X) != 0 {
            user_flags::RX // Executable segment
        } else {
            user_flags::RW_NO_EXEC // Data/BSS - always RW to handle overlaps
        };

        // Calculate number of pages needed
        let start_page = vaddr & !(PAGE_SIZE - 1);
        let end_page = (vaddr + memsz + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let num_pages = (end_page - start_page) / PAGE_SIZE;

        // Allocate and map pages
        for i in 0..num_pages {
            let page_va = start_page + i * PAGE_SIZE;

            // Check if this page is already mapped (from a previous segment)
            let frame_addr = if let Some(&pa) = mapped_pages.get(&page_va) {
                // Reuse existing mapping (all pages are RW now)
                pa
            } else {
                // Allocate a new physical page
                let frame = address_space
                    .alloc_and_map(page_va, page_flags)
                    .map_err(|e| ElfError::MappingFailed(e))?;
                mapped_pages.insert(page_va, frame.addr);
                frame.addr
            };

            // Copy data from ELF file if this page contains file data
            let page_start_in_segment = if page_va >= vaddr { page_va - vaddr } else { 0 };

            if page_start_in_segment < filesz {
                // Calculate how much to copy
                let copy_start = if page_va < vaddr { vaddr - page_va } else { 0 };
                let file_offset = offset + page_start_in_segment;
                let copy_len = core::cmp::min(
                    PAGE_SIZE - copy_start,
                    filesz.saturating_sub(page_start_in_segment),
                );

                if copy_len > 0 && file_offset + copy_len <= elf_data.len() {
                    unsafe {
                        // Convert physical address to kernel virtual address for copy
                        let dst = crate::mmu::phys_to_virt(frame_addr + copy_start);
                        let src = elf_data.as_ptr().add(file_offset);
                        core::ptr::copy_nonoverlapping(src, dst, copy_len);
                    }
                }
            }
            // Pages beyond filesz are already zeroed by alloc_page_zeroed
        }

        // Update brk
        let segment_end = vaddr + memsz;
        if segment_end > brk {
            brk = segment_end;
        }
    }

    // Apply relocations
    // For now we only support R_AARCH64_RELATIVE which is common in TCC binaries
    if let Some(shdrs) = elf.section_headers() {
        for shdr in shdrs {
            if shdr.sh_type == elf::abi::SHT_RELA {
                if let Ok(relas) = elf.section_data_as_relas(&shdr) {
                    for rela in relas {
                        if rela.r_type == R_AARCH64_RELATIVE {
                            let vaddr = rela.r_offset as usize;
                            let addend = rela.r_addend as usize;
                            
                            // Find physical page for this virtual address
                            let page_va = vaddr & !(PAGE_SIZE - 1);
                            if let Some(&pa) = mapped_pages.get(&page_va) {
                                let offset_in_page = vaddr & (PAGE_SIZE - 1);
                                unsafe {
                                    let ptr = crate::mmu::phys_to_virt(pa + offset_in_page) as *mut usize;
                                    // R_AARCH64_RELATIVE: *ptr = B + A
                                    // Since we load at preferred address, B = 0
                                    *ptr = addend;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if DEBUG_ELF_LOADING {
        crate::safe_print!(80, "[ELF] Loaded: entry=0x{:x} brk=0x{:x} pages={}\n",
            entry_point, brk, mapped_pages.len());
    }

    Ok(LoadedElf {
        entry_point,
        address_space,
        brk,
    })
}

/// Convert ELF segment flags to user page flags
fn flags_to_user_flags(elf_flags: u32) -> u64 {
    let readable = (elf_flags & PF_R) != 0;
    let writable = (elf_flags & PF_W) != 0;
    let executable = (elf_flags & PF_X) != 0;

    match (readable, writable, executable) {
        (_, true, true) => user_flags::RW, // RWX -> treat as RW for safety
        (_, true, false) => user_flags::RW_NO_EXEC,
        (true, false, true) => user_flags::RX,
        (true, false, false) => user_flags::RO,
        _ => user_flags::RO, // Default to read-only
    }
}

/// Load an ELF binary and set up user stack with guard page
///
/// # Arguments
/// * `elf_data` - Raw ELF file data
/// * `stack_size` - Size of user stack in bytes (default: 128KB from config::USER_STACK_SIZE)
///
/// # Returns
/// (entry_point, address_space, initial_stack_pointer, brk, stack_bottom, stack_top)
///
/// # Stack Layout (with default 128KB stack)
/// ```text
/// 0x40000000  <- STACK_TOP (unmapped, end of user space)
/// 0x3FFE0000  <- stack_end (top of mapped stack, 32 pages = 128KB)
///    ...      <- stack pages (RW)
/// 0x3FFDF000 + 0x1000 = 0x3FFE0000 <- stack_bottom (first mapped page)
/// 0x3FFDF000  <- guard_page (UNMAPPED - causes fault on overflow)
/// ```
/// Note: Addresses are calculated dynamically based on stack_size parameter.
pub fn load_elf_with_stack(
    elf_data: &[u8],
    stack_size: usize,
) -> Result<(usize, UserAddressSpace, usize, usize, usize, usize), ElfError> {
    let mut loaded = load_elf(elf_data)?;

    // Place stack at top of first 1GB (user space), after mmap region
    // Layout: code (0x400000) < mmap (0x10000000-0x3F000000) < stack (0x3F000000-0x40000000)
    // This keeps everything in the first 1GB where we have fine-grained page table control
    const STACK_TOP: usize = 0x4000_0000; // Top of first 1GB

    // Reserve space for guard page + stack
    // Guard page is at the bottom (lowest address), unmapped to cause fault on overflow
    let total_size = stack_size + PAGE_SIZE; // stack + 1 guard page
    let guard_page = (STACK_TOP - total_size) & !(PAGE_SIZE - 1);
    let stack_bottom = guard_page + PAGE_SIZE; // First usable stack page is above guard

    // Ensure stack is page-aligned
    let stack_bottom_aligned = stack_bottom & !(PAGE_SIZE - 1);
    let stack_pages = (stack_size + PAGE_SIZE - 1) / PAGE_SIZE;

    // Map stack pages (guard page at guard_page is intentionally NOT mapped)
    for i in 0..stack_pages {
        let page_va = stack_bottom_aligned + i * PAGE_SIZE;
        loaded
            .address_space
            .alloc_and_map(page_va, user_flags::RW_NO_EXEC)
            .map_err(|e| ElfError::MappingFailed(e))?;
    }

    // Calculate initial SP within mapped region
    let stack_end = stack_bottom_aligned + stack_pages * PAGE_SIZE;
    let initial_sp_local = (stack_end - 16) & !0xF;

    if DEBUG_ELF_LOADING {
        crate::safe_print!(128, "[ELF] Stack: 0x{:x}-0x{:x} ({} pages), guard=0x{:x}, SP=0x{:x}\n",
            stack_bottom_aligned, stack_end, stack_pages, guard_page, initial_sp_local);
    }

    // Pre-allocate heap pages (64KB = 16 pages, unrolled)
    // Note: Adding more causes EC=0x0 crashes due to binary size constraints
    let hs = (loaded.brk + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let f = user_flags::RW_NO_EXEC;
    let _ = loaded.address_space.alloc_and_map(hs, f);
    let _ = loaded.address_space.alloc_and_map(hs + 0x1000, f);
    let _ = loaded.address_space.alloc_and_map(hs + 0x2000, f);
    let _ = loaded.address_space.alloc_and_map(hs + 0x3000, f);
    let _ = loaded.address_space.alloc_and_map(hs + 0x4000, f);
    let _ = loaded.address_space.alloc_and_map(hs + 0x5000, f);
    let _ = loaded.address_space.alloc_and_map(hs + 0x6000, f);
    let _ = loaded.address_space.alloc_and_map(hs + 0x7000, f);
    let _ = loaded.address_space.alloc_and_map(hs + 0x8000, f);
    let _ = loaded.address_space.alloc_and_map(hs + 0x9000, f);
    let _ = loaded.address_space.alloc_and_map(hs + 0xa000, f);
    let _ = loaded.address_space.alloc_and_map(hs + 0xb000, f);
    let _ = loaded.address_space.alloc_and_map(hs + 0xc000, f);
    let _ = loaded.address_space.alloc_and_map(hs + 0xd000, f);
    let _ = loaded.address_space.alloc_and_map(hs + 0xe000, f);
    let _ = loaded.address_space.alloc_and_map(hs + 0xf000, f);

    if DEBUG_ELF_LOADING {
        crate::safe_print!(64, "[ELF] Heap pre-alloc: 0x{:x} (16 pages)\n", hs);
    }

    // The allocator expects brk(0) to return current brk, then allocates FROM that address.
    // So we need to set brk to the heap START so allocator uses the pre-mapped pages.
    // The kernel's sys_brk will update the brk when allocator calls brk(new_value).

    // Stack pointer starts at top of MAPPED region (grows down)
    // STACK_TOP is the first address ABOVE the stack, so we subtract to get within mapped pages
    // Align to 16 bytes as required by AArch64 ABI
    let stack_end = stack_bottom_aligned + stack_pages * PAGE_SIZE;
    let initial_sp = (stack_end - 16) & !0xF;

    // Return: entry, address_space, initial_sp, brk, stack_bottom, stack_top
    Ok((
        loaded.entry_point,
        loaded.address_space,
        initial_sp,
        hs,
        stack_bottom_aligned,
        stack_end,
    ))
}

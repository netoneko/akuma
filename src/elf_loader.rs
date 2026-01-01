//! ELF Loader
//!
//! Parses and loads ELF binaries into user address space.
//! Uses the `elf` crate for parsing.

use alloc::string::String;
use alloc::vec::Vec;

use elf::abi::{EM_AARCH64, ET_EXEC, PT_LOAD, PF_R, PF_W, PF_X};
use elf::endian::LittleEndian;
use elf::ElfBytes;

use crate::mmu::{user_flags, PageTable, UserAddressSpace, PAGE_SIZE};
use crate::pmm::{self, PhysFrame};

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

        // Determine page flags
        let page_flags = flags_to_user_flags(flags);

        // Calculate number of pages needed
        let start_page = vaddr & !(PAGE_SIZE - 1);
        let end_page = (vaddr + memsz + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let num_pages = (end_page - start_page) / PAGE_SIZE;

        // Allocate and map pages
        for i in 0..num_pages {
            let page_va = start_page + i * PAGE_SIZE;

            // Check if this page is already mapped (from a previous segment)
            let frame_addr = if let Some(&pa) = mapped_pages.get(&page_va) {
                // Reuse existing mapping
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
            let page_start_in_segment = if page_va >= vaddr {
                page_va - vaddr
            } else {
                0
            };

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
                        let dst = (frame_addr + copy_start) as *mut u8;
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

/// Load an ELF binary and set up user stack
///
/// # Arguments
/// * `elf_data` - Raw ELF file data
/// * `stack_size` - Size of user stack in bytes (default: 64KB)
///
/// # Returns
/// (entry_point, address_space, initial_stack_pointer, brk)
pub fn load_elf_with_stack(
    elf_data: &[u8],
    stack_size: usize,
) -> Result<(usize, UserAddressSpace, usize, usize), ElfError> {
    let mut loaded = load_elf(elf_data)?;

    // Place stack at a fixed address in the first 1GB (user space)
    // User stack top at 0x3FFF_F000 (just below 1GB mark)
    // This avoids conflict with kernel RAM mapped at 0x40000000+
    const STACK_TOP: usize = 0x3FFF_F000;
    let stack_bottom = STACK_TOP - stack_size;

    // Ensure stack is page-aligned
    let stack_bottom_aligned = stack_bottom & !(PAGE_SIZE - 1);
    let stack_pages = (stack_size + PAGE_SIZE - 1) / PAGE_SIZE;

    // Map stack pages
    for i in 0..stack_pages {
        let page_va = stack_bottom_aligned + i * PAGE_SIZE;
        loaded
            .address_space
            .alloc_and_map(page_va, user_flags::RW_NO_EXEC)
            .map_err(|e| ElfError::MappingFailed(e))?;
    }

    // Stack pointer starts at top (grows down)
    // Align to 16 bytes as required by AArch64 ABI
    let initial_sp = STACK_TOP & !0xF;

    Ok((loaded.entry_point, loaded.address_space, initial_sp, loaded.brk))
}

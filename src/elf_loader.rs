//! ELF Loader
//!
//! Parses and loads ELF binaries into user address space.
//! Uses the `elf` crate for parsing.

use elf::ElfBytes;
use elf::abi::{EM_AARCH64, ET_EXEC, PF_R, PF_W, PF_X, PT_LOAD, PT_PHDR};
use elf::endian::LittleEndian;

use crate::mmu::{PAGE_SIZE, UserAddressSpace, user_flags};
use alloc::vec::Vec;
use alloc::string::String;

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
    /// Address of program headers in user memory
    pub phdr_addr: usize,
    /// Number of program headers
    pub phnum: usize,
    /// Size of each program header
    pub phent: usize,
}

/// Auxiliary Vector entry types
pub mod auxv {
    pub const AT_NULL: u64 = 0;
    pub const AT_IGNORE: u64 = 1;
    pub const AT_EXECFD: u64 = 2;
    pub const AT_PHDR: u64 = 3;
    pub const AT_PHENT: u64 = 4;
    pub const AT_PHNUM: u64 = 5;
    pub const AT_PAGESZ: u64 = 6;
    pub const AT_BASE: u64 = 7;
    pub const AT_FLAGS: u64 = 8;
    pub const AT_ENTRY: u64 = 9;
    pub const AT_NOTELF: u64 = 10;
    pub const AT_UID: u64 = 11;
    pub const AT_EUID: u64 = 12;
    pub const AT_GID: u64 = 13;
    pub const AT_EGID: u64 = 14;
    pub const AT_RANDOM: u64 = 25;
    pub const AT_HWCAP: u64 = 16;
    pub const AT_CLKTCK: u64 = 17;
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AuxEntry {
    pub a_type: u64,
    pub a_val: u64,
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

    // Track PHDR address
    let mut phdr_addr: usize = 0;

    // Track already-mapped pages (VA -> PA) to avoid double allocation
    let mut mapped_pages: BTreeMap<usize, usize> = BTreeMap::new();

    // Get program headers
    let segments = elf
        .segments()
        .ok_or(ElfError::InvalidFormat("No program headers"))?;

    // Find PT_PHDR if it exists
    for phdr in segments.iter() {
        if phdr.p_type == PT_PHDR {
            phdr_addr = phdr.p_vaddr as usize;
            break;
        }
    }

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

        // Fallback for phdr_addr if PT_PHDR segment was missing
        // Typically phdr is at the very beginning of the first PT_LOAD segment
        if phdr_addr == 0 && offset == 0 {
             phdr_addr = vaddr + elf.ehdr.e_phoff as usize;
        }

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
        phdr_addr,
        phnum: elf.ehdr.e_phnum as usize,
        phent: elf.ehdr.e_phentsize as usize,
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
    args: &[String],
    stack_size: usize,
) -> Result<(usize, UserAddressSpace, usize, usize, usize, usize), ElfError> {
    let mut loaded = load_elf(elf_data)?;

    // Place stack at top of first 1GB (user space)
    const STACK_TOP: usize = 0x4000_0000;

    let total_size = stack_size + PAGE_SIZE;
    let guard_page = (STACK_TOP - total_size) & !(PAGE_SIZE - 1);
    let stack_bottom = guard_page + PAGE_SIZE;
    let stack_pages = (stack_size + PAGE_SIZE - 1) / PAGE_SIZE;

    let mut stack_frames = Vec::new();
    for i in 0..stack_pages {
        let page_va = stack_bottom + i * PAGE_SIZE;
        let frame = loaded
            .address_space
            .alloc_and_map(page_va, user_flags::RW_NO_EXEC)
            .map_err(|e| ElfError::MappingFailed(e))?;
        stack_frames.push(frame);
    }

    // Initial SP at the very top of mapped stack
    let mut sp = STACK_TOP;

    // --- Linux Stack Setup ---
    // 1. Copy argument strings to the top of the stack
    let mut argv_addrs = Vec::new();
    for arg in args.iter().rev() {
        let bytes = arg.as_bytes();
        let len = bytes.len() + 1; // + null terminator
        sp -= len;
        
        let frame_idx = (sp - stack_bottom) / PAGE_SIZE;
        let offset = sp % PAGE_SIZE;
        unsafe {
            let dst = crate::mmu::phys_to_virt(stack_frames[frame_idx].addr + offset);
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst as *mut u8, bytes.len());
            *(dst.add(bytes.len()) as *mut u8) = 0;
        }
        argv_addrs.push(sp);
    }
    argv_addrs.reverse();

    // Align SP to 16 bytes for AArch64 ABI
    sp &= !0xF;

    // 2. Prepare Auxiliary Vector
    let auxv = [
        AuxEntry { a_type: auxv::AT_PHDR, a_val: loaded.phdr_addr as u64 },
        AuxEntry { a_type: auxv::AT_PHNUM, a_val: loaded.phnum as u64 },
        AuxEntry { a_type: auxv::AT_PHENT, a_val: loaded.phent as u64 },
        AuxEntry { a_type: auxv::AT_PAGESZ, a_val: PAGE_SIZE as u64 },
        AuxEntry { a_type: auxv::AT_ENTRY, a_val: loaded.entry_point as u64 },
        AuxEntry { a_type: auxv::AT_NULL, a_val: 0 },
    ];

    // 3. Push everything onto stack in reverse order:
    // [AuxV]
    // [Envp (NULL)]
    // [Argv pointers]
    // [Argc]

    // Calculate space needed
    let auxv_size = auxv.len() * core::mem::size_of::<AuxEntry>();
    let envp_size = 8; // Just NULL
    let argv_size = (args.len() + 1) * 8; // ptrs + NULL
    let argc_size = 8;
    
    let total_ptr_space = auxv_size + envp_size + argv_size + argc_size;
    sp -= total_ptr_space;
    sp &= !0xF; // Re-align

    let mut current_sp = sp;
    let write_stack = |addr: usize, val: u64| {
        let frame_idx = (addr - stack_bottom) / PAGE_SIZE;
        let offset = addr % PAGE_SIZE;
        unsafe {
            let dst = crate::mmu::phys_to_virt(stack_frames[frame_idx].addr + offset) as *mut u64;
            *dst = val;
        }
    };

    // argc
    write_stack(current_sp, args.len() as u64);
    current_sp += 8;

    // argv pointers
    for addr in argv_addrs {
        write_stack(current_sp, addr as u64);
        current_sp += 8;
    }
    write_stack(current_sp, 0); // argv NULL
    current_sp += 8;

    // envp NULL
    write_stack(current_sp, 0);
    current_sp += 8;

    // AuxV
    for entry in auxv {
        write_stack(current_sp, entry.a_type);
        write_stack(current_sp + 8, entry.a_val);
        current_sp += 16;
    }

    // Pre-allocate heap pages (64KB = 16 pages, unrolled)
    let hs = (loaded.brk + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let f = user_flags::RW_NO_EXEC;
    for i in 0..16 {
        let _ = loaded.address_space.alloc_and_map(hs + i * 0x1000, f);
    }

    if DEBUG_ELF_LOADING {
        crate::safe_print!(64, "[ELF] Heap pre-alloc: 0x{:x} (16 pages)\n", hs);
        crate::safe_print!(128, "[ELF] Stack: 0x{:x}-0x{:x}, SP=0x{:x}, argc={}\n",
            stack_bottom, STACK_TOP, sp, args.len());
    }

    // Return: entry, address_space, initial_sp, brk, stack_bottom, stack_top
    Ok((
        loaded.entry_point,
        loaded.address_space,
        sp, // initial_sp
        hs,
        stack_bottom,
        STACK_TOP,
    ))
}

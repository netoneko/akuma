//! ELF Loader
//!
//! Parses and loads ELF binaries into user address space.
//! Uses the `elf` crate for parsing.

use elf::ElfBytes;
use elf::abi::{EM_AARCH64, ET_EXEC, PF_R, PF_W, PF_X, PT_INTERP, PT_LOAD, PT_PHDR};
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
    /// Binary requires a dynamic linker (not statically linked)
    DynamicallyLinked,
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
            ElfError::DynamicallyLinked => write!(f, "Dynamically linked binary requires interpreter (recompile with -static)"),
            ElfError::OutOfMemory => write!(f, "Out of memory"),
            ElfError::AddressSpaceFailed => write!(f, "Failed to create address space"),
            ElfError::MappingFailed(msg) => write!(f, "Mapping failed: {}", msg),
        }
    }
}

use alloc::collections::BTreeMap;

const R_AARCH64_ABS64: u32 = 257;
const R_AARCH64_GLOB_DAT: u32 = 1025;
const R_AARCH64_JUMP_SLOT: u32 = 1026;
const R_AARCH64_RELATIVE: u32 = 1027;

/// Load an ELF binary from memory
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

    // Reject dynamically-linked binaries that require a real interpreter.
    // Static-PIE binaries may have PT_INTERP with an empty string (1-byte null) — allow those.
    for phdr in segments.iter() {
        if phdr.p_type == PT_INTERP && phdr.p_filesz > 1 {
            crate::safe_print!(96, "[ELF] Error: binary requires dynamic linker, recompile with -static\n");
            return Err(ElfError::DynamicallyLinked);
        }
    }

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

        let page_flags = if (flags & PF_X) != 0 {
            user_flags::RX
        } else {
            user_flags::RW_NO_EXEC
        };

        let start_page = vaddr & !(PAGE_SIZE - 1);
        let end_page = (vaddr + memsz + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let num_pages = (end_page - start_page) / PAGE_SIZE;

        for i in 0..num_pages {
            let page_va = start_page + i * PAGE_SIZE;

            let frame_addr = if let Some(&pa) = mapped_pages.get(&page_va) {
                pa
            } else {
                let frame = address_space
                    .alloc_and_map(page_va, page_flags)
                    .map_err(|e| ElfError::MappingFailed(e))?;
                mapped_pages.insert(page_va, frame.addr);
                frame.addr
            };

            let page_start_in_segment = if page_va >= vaddr { page_va - vaddr } else { 0 };

            if page_start_in_segment < filesz {
                let copy_start = if page_va < vaddr { vaddr - page_va } else { 0 };
                let file_offset = offset + page_start_in_segment;
                let copy_len = core::cmp::min(
                    PAGE_SIZE - copy_start,
                    filesz.saturating_sub(page_start_in_segment),
                );

                if copy_len > 0 && file_offset + copy_len <= elf_data.len() {
                    unsafe {
                        let dst = crate::mmu::phys_to_virt(frame_addr + copy_start);
                        let src = elf_data.as_ptr().add(file_offset);
                        core::ptr::copy_nonoverlapping(src, dst, copy_len);
                    }
                }
            }
        }

        let segment_end = vaddr + memsz;
        if segment_end > brk {
            brk = segment_end;
        }
    }

    // Apply relocations
    if let Some(shdrs) = elf.section_headers() {
        let dynsyms = elf.dynamic_symbol_table().ok().flatten();

        for shdr in shdrs {
            if shdr.sh_type == elf::abi::SHT_RELA {
                if let Ok(relas) = elf.section_data_as_relas(&shdr) {
                    for rela in relas {
                        let r_type = rela.r_type;
                        let vaddr = rela.r_offset as usize;
                        let addend = rela.r_addend as usize;
                        let sym_idx = rela.r_sym as usize;

                        let mut sym_value = 0;
                        if sym_idx != 0 {
                            if let Some(ref syms) = dynsyms {
                                if let Ok(sym) = syms.0.get(sym_idx) {
                                    sym_value = sym.st_value as usize;
                                }
                            }
                        }

                        let page_va = vaddr & !(PAGE_SIZE - 1);
                        if let Some(&pa) = mapped_pages.get(&page_va) {
                            let offset_in_page = vaddr & (PAGE_SIZE - 1);
                            unsafe {
                                let ptr = crate::mmu::phys_to_virt(pa + offset_in_page) as *mut usize;
                                match r_type {
                                    R_AARCH64_RELATIVE => {
                                        *ptr = addend;
                                    }
                                    R_AARCH64_ABS64 | R_AARCH64_GLOB_DAT | R_AARCH64_JUMP_SLOT => {
                                        *ptr = sym_value + addend;
                                    }
                                    _ => {}
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

/// Helper to build a userspace stack according to Linux AArch64 ABI
pub struct UserStack {
    pub stack_bottom: usize,
    pub stack_top: usize,
    pub sp: usize,
    pub frames: Vec<crate::pmm::PhysFrame>,
}

impl UserStack {
    pub fn new(stack_bottom: usize, stack_top: usize, frames: Vec<crate::pmm::PhysFrame>) -> Self {
        Self {
            stack_bottom,
            stack_top,
            sp: stack_top,
            frames,
        }
    }

    pub fn push_str(&mut self, s: &str) -> usize {
        let bytes = s.as_bytes();
        let len = bytes.len() + 1;
        self.sp -= len;
        
        // Copy string byte-by-byte or in chunks to handle page boundaries correctly
        let mut written = 0;
        while written < bytes.len() {
            let va = self.sp + written;
            let frame_idx = (va - self.stack_bottom) / PAGE_SIZE;
            let offset = va % PAGE_SIZE;
            let chunk_len = core::cmp::min(bytes.len() - written, PAGE_SIZE - offset);
            
            unsafe {
                let dst = crate::mmu::phys_to_virt(self.frames[frame_idx].addr + offset);
                core::ptr::copy_nonoverlapping(bytes.as_ptr().add(written), dst as *mut u8, chunk_len);
            }
            written += chunk_len;
        }
        
        // Null terminator
        let va = self.sp + bytes.len();
        let frame_idx = (va - self.stack_bottom) / PAGE_SIZE;
        let offset = va % PAGE_SIZE;
        unsafe {
            let dst = crate::mmu::phys_to_virt(self.frames[frame_idx].addr + offset) as *mut u8;
            *dst = 0;
        }
        
        self.sp
    }

    pub fn push_u64(&mut self, val: u64) {
        self.sp -= 8;
        // Since SP was aligned to 8 or 16, a u64 won't cross a 4KB boundary
        let frame_idx = (self.sp - self.stack_bottom) / PAGE_SIZE;
        let offset = self.sp % PAGE_SIZE;
        unsafe {
            let dst = crate::mmu::phys_to_virt(self.frames[frame_idx].addr + offset) as *mut u64;
            *dst = val;
        }
    }

    pub fn push_raw(&mut self, data: &[u8]) -> usize {
        self.sp -= data.len();
        
        let mut written = 0;
        while written < data.len() {
            let va = self.sp + written;
            let frame_idx = (va - self.stack_bottom) / PAGE_SIZE;
            let offset = va % PAGE_SIZE;
            let chunk_len = core::cmp::min(data.len() - written, PAGE_SIZE - offset);
            
            unsafe {
                let dst = crate::mmu::phys_to_virt(self.frames[frame_idx].addr + offset);
                core::ptr::copy_nonoverlapping(data.as_ptr().add(written), dst as *mut u8, chunk_len);
            }
            written += chunk_len;
        }
        self.sp
    }

    pub fn align_sp(&mut self, alignment: usize) {
        self.sp &= !(alignment - 1);
    }
}

pub fn setup_linux_stack(
    stack: &mut UserStack,
    args: &[String],
    env: &[String],
    auxv: &[AuxEntry],
) -> usize {
    // Calculate total number of 8-byte words to be pushed
    // argc: 1
    // argv: args.len() + 1 (NULL)
    // envp: env.len() + 1 (NULL)
    // auxv: 2 * (auxv.len() + 1) (each entry is 2 words, + NULL entry)
    let total_words = 1 + (args.len() + 1) + (env.len() + 1) + 2 * (auxv.len() + 1);
    
    // Standard AArch64 Linux ABI requires SP to be 16-byte aligned.
    // If total_words is ODD, we need one word of padding at the top (highest address)
    // to ensure SP (at the lowest address) ends up 16-byte aligned.
    stack.align_sp(16);
    if total_words % 2 != 0 {
        stack.push_u64(0); // Alignment padding
    }

    let mut envp_addrs = Vec::new();
    for e in env.iter().rev() {
        envp_addrs.push(stack.push_str(e));
    }
    envp_addrs.reverse();

    let mut argv_addrs = Vec::new();
    for a in args.iter().rev() {
        argv_addrs.push(stack.push_str(a));
    }
    argv_addrs.reverse();

    stack.align_sp(16);

    // Push Auxiliary Vector
    stack.push_u64(0); // AT_NULL a_type
    stack.push_u64(0); // AT_NULL a_val
    for entry in auxv.iter().rev() {
        stack.push_u64(entry.a_val);
        stack.push_u64(entry.a_type);
    }

    // Push envp NULL and pointers
    stack.push_u64(0);
    for addr in envp_addrs.iter().rev() {
        stack.push_u64(*addr as u64);
    }

    // Push argv NULL and pointers
    stack.push_u64(0);
    for addr in argv_addrs.iter().rev() {
        stack.push_u64(*addr as u64);
    }

    // Push argc
    stack.push_u64(args.len() as u64);

    stack.sp
}

pub fn load_elf_with_stack(
    elf_data: &[u8],
    args: &[String],
    env: &[String],
    stack_size: usize,
) -> Result<(usize, UserAddressSpace, usize, usize, usize, usize), ElfError> {
    let mut loaded = load_elf(elf_data)?;
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

    let mut stack = UserStack::new(stack_bottom, STACK_TOP, stack_frames);
    let random_ptr = stack.push_raw(&[0u8; 16]);

    let auxv = [
        AuxEntry { a_type: auxv::AT_PHDR, a_val: loaded.phdr_addr as u64 },
        AuxEntry { a_type: auxv::AT_PHNUM, a_val: loaded.phnum as u64 },
        AuxEntry { a_type: auxv::AT_PHENT, a_val: loaded.phent as u64 },
        AuxEntry { a_type: auxv::AT_PAGESZ, a_val: PAGE_SIZE as u64 },
        AuxEntry { a_type: auxv::AT_ENTRY, a_val: loaded.entry_point as u64 },
        AuxEntry { a_type: auxv::AT_CLKTCK, a_val: 100 },
        AuxEntry { a_type: auxv::AT_RANDOM, a_val: random_ptr as u64 },
        AuxEntry { a_type: auxv::AT_UID, a_val: 0 },
        AuxEntry { a_type: auxv::AT_EUID, a_val: 0 },
        AuxEntry { a_type: auxv::AT_GID, a_val: 0 },
        AuxEntry { a_type: auxv::AT_EGID, a_val: 0 },
    ];

    let sp = setup_linux_stack(&mut stack, args, env, &auxv);

    let hs = (loaded.brk + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    for i in 0..16 {
        let _ = loaded.address_space.alloc_and_map(hs + i * 0x1000, user_flags::RW_NO_EXEC);
    }

    if DEBUG_ELF_LOADING {
        crate::safe_print!(64, "[ELF] Heap pre-alloc: 0x{:x} (16 pages)\n", hs);
        crate::safe_print!(128, "[ELF] Stack: 0x{:x}-0x{:x}, SP=0x{:x}, argc={}\n",
            stack_bottom, STACK_TOP, sp, args.len());
    }

    Ok((loaded.entry_point, loaded.address_space, sp, hs, stack_bottom, STACK_TOP))
}

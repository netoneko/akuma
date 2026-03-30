//! ELF Loader
//!
//! Parses and loads ELF binaries into user address space.

pub mod types;

pub use types::*;

use elf::ElfBytes;
use elf::abi::{EM_AARCH64, ET_DYN, ET_EXEC, PF_R, PF_W, PF_X, PT_INTERP, PT_LOAD, PT_PHDR};
use elf::endian::LittleEndian;

use crate::mmu::{PAGE_SIZE, UserAddressSpace, user_flags};
use crate::runtime::runtime;
use alloc::vec::Vec;
use alloc::string::String;
use alloc::collections::BTreeMap;

/// Result of loading an ELF binary
pub struct LoadedElf {
    pub entry_point: usize,
    pub address_space: UserAddressSpace,
    pub brk: usize,
    pub phdr_addr: usize,
    pub phnum: usize,
    pub phent: usize,
    pub interp: Option<InterpInfo>,
    pub deferred_segments: Vec<DeferredLazySegment>,
}

/// Load an ELF binary from memory.
/// `interp_prefix` is prepended to the PT_INTERP path when loading the dynamic
/// linker (used for container rootfs where the interpreter lives under a prefix).
pub fn load_elf(elf_data: &[u8], interp_prefix: Option<&str>) -> Result<LoadedElf, ElfError> {
    // Parse ELF header
    let elf = match ElfBytes::<LittleEndian>::minimal_parse(elf_data) {
        Ok(e) => e,
        Err(_) => {
            let mut magic = [0u8; 4];
            if elf_data.len() >= 4 {
                magic.copy_from_slice(&elf_data[0..4]);
            }
            return Err(ElfError::InvalidMagic(magic));
        }
    };

    // Verify architecture
    if elf.ehdr.e_machine != EM_AARCH64 {
        return Err(ElfError::WrongArchitecture);
    }

    // Accept ET_EXEC (normal static) and ET_DYN (static-PIE)
    let is_pie = elf.ehdr.e_type == ET_DYN;
    if elf.ehdr.e_type != ET_EXEC && !is_pie {
        return Err(ElfError::NotExecutable);
    }

    // Static-PIE binaries have p_vaddr starting near 0; load at a fixed base.
    const PIE_BASE: usize = 0x1000_0000;
    let base = if is_pie { PIE_BASE } else { 0 };

    let entry_point = base + elf.ehdr.e_entry as usize;

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

    // Check for PT_INTERP — if present, we need to load the dynamic linker.
    // Static-PIE binaries may have PT_INTERP with an empty string (1-byte null) — skip those.
    let mut interp_path: Option<String> = None;
    for phdr in segments.iter() {
        if phdr.p_type == PT_INTERP && phdr.p_filesz > 1 {
            let off = phdr.p_offset as usize;
            let sz = phdr.p_filesz as usize;
            if off + sz <= elf_data.len() {
                let raw = &elf_data[off..off + sz];
                let path_bytes = if raw.last() == Some(&0) { &raw[..raw.len() - 1] } else { raw };
                if let Ok(s) = core::str::from_utf8(path_bytes) {
                    interp_path = Some(String::from(s));
                }
            }
        }
    }

    // Find PT_PHDR if it exists
    for phdr in segments.iter() {
        if phdr.p_type == PT_PHDR {
            phdr_addr = base + phdr.p_vaddr as usize;
            break;
        }
    }

    // Load each PT_LOAD segment
    for phdr in segments.iter() {
        if phdr.p_type != PT_LOAD {
            continue;
        }

        let vaddr = base + phdr.p_vaddr as usize;
        let memsz = phdr.p_memsz as usize;
        let filesz = phdr.p_filesz as usize;
        let offset = phdr.p_offset as usize;
        let flags = phdr.p_flags;

        // Fallback for phdr_addr if PT_PHDR segment was missing
        if phdr_addr == 0 && phdr.p_offset == 0 {
             phdr_addr = vaddr + elf.ehdr.e_phoff as usize;
        }

        if DEBUG_ELF_LOADING {
            log::debug!("[ELF] Segment: VA=0x{:08x} filesz=0x{:x} memsz=0x{:x} flags={}{}{}",
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

    // Apply relocations for ET_EXEC only.
    // Static-PIE (ET_DYN) binaries self-relocate at startup via musl's _dlstart_c.
    if !is_pie {
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
    } // !is_pie

    if DEBUG_ELF_LOADING {
        log::debug!("[ELF] Loaded: entry=0x{:x} brk=0x{:x} pages={}",
            entry_point, brk, mapped_pages.len());
    }

    let interp = if let Some(ref ipath) = interp_path {
        let resolved_interp = match interp_prefix {
            Some(prefix) => {
                let mut p = String::from(prefix);
                if !p.ends_with('/') && !ipath.starts_with('/') { p.push('/'); }
                p.push_str(ipath);
                p
            }
            None => ipath.clone(),
        };
        if DEBUG_ELF_LOADING {
            log::debug!("[ELF] Loading interpreter: {}", resolved_interp);
        }
        let interp_data = (runtime().read_file)(&resolved_interp)
            .map_err(|_| ElfError::InvalidFormat("Cannot read interpreter"))?;
        let interp_info = load_interpreter(&interp_data, &mut address_space)?;
        if DEBUG_ELF_LOADING {
            log::debug!("[ELF] Interpreter loaded at base=0x{:x} entry=0x{:x}",
                interp_info.base_addr, interp_info.entry_point);
        }
        Some(interp_info)
    } else {
        None
    };

    Ok(LoadedElf {
        entry_point,
        address_space,
        brk,
        phdr_addr,
        phnum: elf.ehdr.e_phnum as usize,
        phent: elf.ehdr.e_phentsize as usize,
        interp,
        deferred_segments: Vec::new(),
    })
}

/// Load the dynamic linker (interpreter) ELF into an existing address space.
fn load_interpreter(elf_data: &[u8], address_space: &mut UserAddressSpace) -> Result<InterpInfo, ElfError> {
    let elf = ElfBytes::<LittleEndian>::minimal_parse(elf_data)
        .map_err(|_| ElfError::InvalidFormat("Interpreter parse failed"))?;

    if elf.ehdr.e_machine != EM_AARCH64 {
        return Err(ElfError::WrongArchitecture);
    }

    let base = INTERP_BASE;
    let entry_point = base + elf.ehdr.e_entry as usize;

    let segments = elf
        .segments()
        .ok_or(ElfError::InvalidFormat("Interpreter has no program headers"))?;

    let mut mapped_pages: alloc::collections::BTreeMap<usize, usize> = alloc::collections::BTreeMap::new();

    for phdr in segments.iter() {
        if phdr.p_type != PT_LOAD { continue; }

        let vaddr = base + phdr.p_vaddr as usize;
        let memsz = phdr.p_memsz as usize;
        let filesz = phdr.p_filesz as usize;
        let offset = phdr.p_offset as usize;
        let flags = phdr.p_flags;

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
    }

    // Apply relocations so the interpreter can self-bootstrap.
    // Process both .rela.dyn (DT_RELA) and .rela.plt (DT_JMPREL).
    let dynsyms = elf.dynamic_symbol_table().ok().flatten();
    if let Some(shdrs) = elf.section_headers() {
        let mut rela_count = 0usize;
        for shdr in shdrs {
            if shdr.sh_type != elf::abi::SHT_RELA { continue; }
            if let Ok(relas) = elf.section_data_as_relas(&shdr) {
                for rela in relas {
                    let r_type = rela.r_type;
                    let vaddr = base + rela.r_offset as usize;
                    let addend = rela.r_addend as usize;
                    let sym_idx = rela.r_sym as usize;

                    let page_va = vaddr & !(PAGE_SIZE - 1);
                    if let Some(&pa) = mapped_pages.get(&page_va) {
                        let offset_in_page = vaddr & (PAGE_SIZE - 1);
                        let ptr = unsafe {
                            crate::mmu::phys_to_virt(pa + offset_in_page) as *mut usize
                        };

                        match r_type {
                            R_AARCH64_RELATIVE => {
                                unsafe { *ptr = base + addend; }
                                rela_count += 1;
                            }
                            R_AARCH64_GLOB_DAT | R_AARCH64_JUMP_SLOT => {
                                if sym_idx != 0 {
                                    if let Some(ref syms) = dynsyms {
                                        if let Ok(sym) = syms.0.get(sym_idx) {
                                            unsafe { *ptr = base + sym.st_value as usize + addend; }
                                            rela_count += 1;
                                        }
                                    }
                                }
                            }
                            R_AARCH64_ABS64 => {
                                if sym_idx != 0 {
                                    if let Some(ref syms) = dynsyms {
                                        if let Ok(sym) = syms.0.get(sym_idx) {
                                            unsafe { *ptr = base + sym.st_value as usize + addend; }
                                            rela_count += 1;
                                        }
                                    }
                                } else {
                                    unsafe { *ptr = base + addend; }
                                    rela_count += 1;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        if DEBUG_ELF_LOADING {
            log::debug!("[ELF] Interpreter: applied {} relocations", rela_count);
        }
    }

    if DEBUG_ELF_LOADING {
        log::debug!("[ELF] Interpreter: entry=0x{:x} pages={}", entry_point, mapped_pages.len());
    }

    Ok(InterpInfo { entry_point, base_addr: base })
}

/// Helper to build a userspace stack according to Linux AArch64 ABI
pub struct UserStack {
    pub stack_bottom: usize,
    pub stack_top: usize,
    pub sp: usize,
    pub frames: Vec<crate::runtime::PhysFrame>,
}

impl UserStack {
    pub fn new(stack_bottom: usize, stack_top: usize, frames: Vec<crate::runtime::PhysFrame>) -> Self {
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

/// Compute user stack top address dynamically based on binary layout.
///
/// Truly tiny static binaries (musl/TCC C programs, typically < 200 KB) get
/// the default 1 GB address space.  Any static binary >= 512 KB — in
/// particular Go programs, whose embedded runtime is ~1–3 MB minimum — gets
/// the same large VA space (128 GB mmap + 256 GB stack top) as dynamically-
/// linked binaries.
///
/// Threshold rationale: the Go runtime probes heap arena addresses
/// (`arenaHints`) via `mmap(hint=4GB+k*64MB, PROT_NONE)`.  On Akuma the
/// kernel ignores hints and returns the next available VA; Go then munmaps
/// the wrong address.  PROT_NONE frees do NOT recycle VA (to prevent
/// infinite mmap→reject→munmap loops), so each probe permanently consumes
/// 64 MB.  With 1 GB of VA: 1 GB / 64 MB ≈ 15 probes before exhaustion —
/// Go tries up to 128 hints and panics with "out of memory".  At 512 KB the
/// threshold sits safely between tiny C programs (< 200 KB) and the smallest
/// possible Go binary (> 1 MB).
fn compute_stack_top(brk: usize, has_interp: bool) -> usize {
    const DEFAULT: usize = 0x4000_0000; // 1 GB — for truly tiny static binaries
    const SMALL_STATIC_THRESHOLD: usize = 0x8_0000; // 512 KB

    if !has_interp && brk < SMALL_STATIC_THRESHOLD {
        return DEFAULT;
    }

    const INTERP_END: usize = 0x3010_0000;
    const MIN_MMAP_SPACE: usize = 0x20_0000_0000; // 128GB for large/dynamic binaries (JSC gigacage needs 128GB)
    const MAX_STACK_TOP: usize = 0x40_0000_0000; // 256GB — well within 48-bit VA (T0SZ=16)

    let base_mmap = (brk + 0x1000_0000) & !0xFFFF; // brk + 256MB gap
    let mmap_start = if has_interp {
        core::cmp::max(base_mmap, INTERP_END)
    } else {
        base_mmap
    };

    let needed = mmap_start + MIN_MMAP_SPACE;
    let raw = core::cmp::max(DEFAULT, needed);
    let aligned = (raw + 0x0FFF_FFFF) & !0x0FFF_FFFF;
    core::cmp::min(aligned, MAX_STACK_TOP)
}

pub fn load_elf_with_stack(
    elf_data: &[u8],
    args: &[String],
    env: &[String],
    stack_size: usize,
    interp_prefix: Option<&str>,
) -> Result<(usize, UserAddressSpace, usize, usize, usize, usize, usize, Vec<DeferredLazySegment>), ElfError> {
    let mut loaded = load_elf(elf_data, interp_prefix)?;
    let has_interp = loaded.interp.is_some();
    let stack_top = compute_stack_top(loaded.brk, has_interp);
    let mmap_floor = if has_interp { 0x3010_0000 } else { 0 };
    let total_size = stack_size + PAGE_SIZE;
    let guard_page = (stack_top - total_size) & !(PAGE_SIZE - 1);
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

    let mut stack = UserStack::new(stack_bottom, stack_top, stack_frames);
    let random_ptr = stack.push_raw(&[0u8; 16]);

    let actual_entry = if let Some(ref interp) = loaded.interp {
        interp.entry_point
    } else {
        loaded.entry_point
    };

    let mut auxv_vec = Vec::new();
    auxv_vec.push(AuxEntry { a_type: auxv::AT_PHDR, a_val: loaded.phdr_addr as u64 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_PHNUM, a_val: loaded.phnum as u64 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_PHENT, a_val: loaded.phent as u64 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_PAGESZ, a_val: PAGE_SIZE as u64 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_ENTRY, a_val: loaded.entry_point as u64 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_CLKTCK, a_val: 100 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_RANDOM, a_val: random_ptr as u64 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_UID, a_val: 0 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_EUID, a_val: 0 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_GID, a_val: 0 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_EGID, a_val: 0 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_HWCAP, a_val: auxv::AARCH64_HWCAP });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_HWCAP2, a_val: 0 });
    if let Some(ref interp) = loaded.interp {
        auxv_vec.push(AuxEntry { a_type: auxv::AT_BASE, a_val: interp.base_addr as u64 });
    }

    let sp = setup_linux_stack(&mut stack, args, env, &auxv_vec);

    let hs = (loaded.brk + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    for i in 0..16 {
        let _ = loaded.address_space.alloc_and_map(hs + i * 0x1000, user_flags::RW_NO_EXEC);
    }

    if DEBUG_ELF_LOADING {
        log::debug!("[ELF] Heap pre-alloc: 0x{:x} (16 pages)", hs);
        log::debug!("[ELF] Stack: 0x{:x}-0x{:x}, SP=0x{:x}, argc={}",
            stack_bottom, stack_top, sp, args.len());
        if loaded.interp.is_some() {
            log::debug!("[ELF] Dynamic: start at interpreter 0x{:x}, AT_ENTRY=0x{:x}",
                actual_entry, loaded.entry_point);
        }
    }

    Ok((actual_entry, loaded.address_space, sp, hs, stack_bottom, stack_top, mmap_floor, Vec::new()))
}

// ============================================================================
// On-demand ELF loading from file path (for large binaries)
// ============================================================================

fn parse_elf64_ehdr_checked(buf: &[u8]) -> Result<Elf64Ehdr, ElfError> {
    if buf.len() < ELF64_EHDR_SIZE {
        return Err(ElfError::InvalidFormat("Header too short"));
    }
    if buf[0..4] != ELF_MAGIC {
        return Err(ElfError::InvalidFormat("Bad magic"));
    }
    if buf[4] != ELFCLASS64 {
        return Err(ElfError::InvalidFormat("Not ELF64"));
    }
    if buf[5] != ELFDATA2LSB {
        return Err(ElfError::InvalidFormat("Not little-endian"));
    }
    Ok(Elf64Ehdr {
        e_type: read_u16_le(buf, 16),
        e_machine: read_u16_le(buf, 18),
        e_entry: read_u64_le(buf, 24),
        e_phoff: read_u64_le(buf, 32),
        e_phentsize: read_u16_le(buf, 54),
        e_phnum: read_u16_le(buf, 56),
    })
}


/// Read exactly `len` bytes from a file at `offset`, returning an error on short reads.
fn file_read_exact(path: &str, offset: usize, len: usize) -> Result<Vec<u8>, ElfError> {
    let mut buf = alloc::vec![0u8; len];
    let n = (runtime().read_at)(path, offset, &mut buf)
        .map_err(|_| ElfError::InvalidFormat("File read failed"))?;
    if n < len {
        return Err(ElfError::InvalidFormat("Short read"));
    }
    Ok(buf)
}

/// Load an ELF binary on demand from a file path, reading segment data
/// page-by-page via read_at() instead of buffering the entire file.
/// Supports PIE (ET_DYN) and non-PIE (ET_EXEC) without relocations.
pub fn load_elf_from_path(path: &str, file_size: usize, interp_prefix: Option<&str>) -> Result<LoadedElf, ElfError> {
    let hdr_buf = file_read_exact(path, 0, ELF64_EHDR_SIZE)?;
    let ehdr = parse_elf64_ehdr_checked(&hdr_buf)?;

    if ehdr.e_machine != EM_AARCH64 as u16 {
        return Err(ElfError::WrongArchitecture);
    }

    let is_pie = ehdr.e_type == ET_DYN as u16;
    if ehdr.e_type != ET_EXEC as u16 && !is_pie {
        return Err(ElfError::NotExecutable);
    }

    const PIE_BASE: usize = 0x1000_0000;
    let base = if is_pie { PIE_BASE } else { 0 };
    let entry_point = base + ehdr.e_entry as usize;

    let mut address_space = UserAddressSpace::new().ok_or(ElfError::AddressSpaceFailed)?;
    let mut brk: usize = 0;
    let mut phdr_addr: usize = 0;
    let mut interp_path: Option<String> = None;
    let mut deferred_segments: Vec<DeferredLazySegment> = Vec::new();

    let phdr_table_size = ehdr.e_phnum as usize * ehdr.e_phentsize as usize;
    let phdr_buf = file_read_exact(path, ehdr.e_phoff as usize, phdr_table_size)?;

    let mut phdrs = Vec::with_capacity(ehdr.e_phnum as usize);
    for i in 0..ehdr.e_phnum as usize {
        let off = i * ehdr.e_phentsize as usize;
        if let Some(phdr) = parse_elf64_phdr(&phdr_buf[off..]) {
            phdrs.push(phdr);
        }
    }

    for phdr in &phdrs {
        if phdr.p_type == PT_INTERP && phdr.p_filesz > 1 {
            let interp_data = file_read_exact(path, phdr.p_offset as usize, phdr.p_filesz as usize)?;
            let raw = if interp_data.last() == Some(&0) {
                &interp_data[..interp_data.len() - 1]
            } else {
                &interp_data[..]
            };
            if let Ok(s) = core::str::from_utf8(raw) {
                interp_path = Some(String::from(s));
            }
        }
        if phdr.p_type == PT_PHDR {
            phdr_addr = base + phdr.p_vaddr as usize;
        }
    }

    let file_inode = (runtime().resolve_inode)(path).unwrap_or(0);

    if DEBUG_ELF_LOADING {
        log::debug!("[ELF] On-demand loading from path, file_size={} ({}MB), is_pie={}, inode={}",
            file_size, file_size / 1024 / 1024, is_pie, file_inode);
    }

    for phdr in &phdrs {
        if phdr.p_type != PT_LOAD {
            continue;
        }

        let vaddr = base + phdr.p_vaddr as usize;
        let memsz = phdr.p_memsz as usize;
        let filesz = phdr.p_filesz as usize;
        let offset = phdr.p_offset as usize;
        let flags = phdr.p_flags;

        if phdr_addr == 0 && phdr.p_offset == 0 {
            phdr_addr = vaddr + ehdr.e_phoff as usize;
        }

        if DEBUG_ELF_LOADING {
            log::debug!("[ELF] Segment (deferred): VA=0x{:08x} filesz=0x{:x} memsz=0x{:x} flags={}{}{}",
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
        let seg_size = end_page - start_page;

        deferred_segments.push(DeferredLazySegment {
            start_va: start_page,
            size: seg_size,
            page_flags,
            file_source: Some(FileSegmentSource {
                path: String::from(path),
                inode: file_inode,
                file_offset: offset,
                filesz,
                segment_va: vaddr,
            }),
        });

        let segment_end = vaddr + memsz;
        if segment_end > brk {
            brk = segment_end;
        }
    }

    // Register gap regions between PT_LOAD segments as zero-fill lazy regions.
    let mut load_segments: Vec<(usize, usize)> = phdrs.iter()
        .filter(|p| p.p_type == PT_LOAD)
        .map(|p| {
            let va = base + p.p_vaddr as usize;
            let end = va + p.p_memsz as usize;
            let page_start = va & !(PAGE_SIZE - 1);
            let page_end = (end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
            (page_start, page_end)
        })
        .collect();
    load_segments.sort_by_key(|&(start, _)| start);

    for w in load_segments.windows(2) {
        let prev_end = w[0].1;
        let next_start = w[1].0;
        if prev_end < next_start {
            let gap_size = next_start - prev_end;
            deferred_segments.push(DeferredLazySegment {
                start_va: prev_end,
                size: gap_size,
                page_flags: user_flags::RW_NO_EXEC,
                file_source: None,
            });
            if DEBUG_ELF_LOADING {
                log::debug!("[ELF] Gap region (deferred): 0x{:08x}-0x{:08x} ({} pages)",
                    prev_end, next_start, gap_size / PAGE_SIZE);
            }
        }
    }

    if DEBUG_ELF_LOADING {
        log::debug!("[ELF] Deferred: entry=0x{:x} brk=0x{:x} segments={}",
            entry_point, brk, deferred_segments.len());
    }

    let interp = if let Some(ref ipath) = interp_path {
        let resolved_interp = match interp_prefix {
            Some(prefix) => {
                let mut p = String::from(prefix);
                if !p.ends_with('/') && !ipath.starts_with('/') { p.push('/'); }
                p.push_str(ipath);
                p
            }
            None => ipath.clone(),
        };
        if DEBUG_ELF_LOADING {
            log::debug!("[ELF] Loading interpreter: {}", resolved_interp);
        }
        let interp_data = (runtime().read_file)(&resolved_interp)
            .map_err(|_| ElfError::InvalidFormat("Cannot read interpreter"))?;
        let interp_info = load_interpreter(&interp_data, &mut address_space)?;
        if DEBUG_ELF_LOADING {
            log::debug!("[ELF] Interpreter loaded at base=0x{:x} entry=0x{:x}",
                interp_info.base_addr, interp_info.entry_point);
        }
        Some(interp_info)
    } else {
        None
    };

    Ok(LoadedElf {
        entry_point,
        address_space,
        brk,
        phdr_addr,
        phnum: ehdr.e_phnum as usize,
        phent: ehdr.e_phentsize as usize,
        interp,
        deferred_segments,
    })
}

pub fn load_elf_with_stack_from_path(
    path: &str,
    file_size: usize,
    args: &[String],
    env: &[String],
    stack_size: usize,
    interp_prefix: Option<&str>,
) -> Result<(usize, UserAddressSpace, usize, usize, usize, usize, usize, Vec<DeferredLazySegment>), ElfError> {
    let mut loaded = load_elf_from_path(path, file_size, interp_prefix)?;
    let has_interp = loaded.interp.is_some();
    let stack_top = compute_stack_top(loaded.brk, has_interp);
    let mmap_floor = if has_interp { 0x3010_0000 } else { 0 };
    let total_size = stack_size + PAGE_SIZE;
    let guard_page = (stack_top - total_size) & !(PAGE_SIZE - 1);
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

    let mut stack = UserStack::new(stack_bottom, stack_top, stack_frames);
    let random_ptr = stack.push_raw(&[0u8; 16]);

    let actual_entry = if let Some(ref interp) = loaded.interp {
        interp.entry_point
    } else {
        loaded.entry_point
    };

    let mut auxv_vec = Vec::new();
    auxv_vec.push(AuxEntry { a_type: auxv::AT_PHDR, a_val: loaded.phdr_addr as u64 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_PHNUM, a_val: loaded.phnum as u64 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_PHENT, a_val: loaded.phent as u64 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_PAGESZ, a_val: PAGE_SIZE as u64 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_ENTRY, a_val: loaded.entry_point as u64 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_CLKTCK, a_val: 100 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_RANDOM, a_val: random_ptr as u64 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_UID, a_val: 0 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_EUID, a_val: 0 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_GID, a_val: 0 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_EGID, a_val: 0 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_HWCAP, a_val: auxv::AARCH64_HWCAP });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_HWCAP2, a_val: 0 });
    if let Some(ref interp) = loaded.interp {
        auxv_vec.push(AuxEntry { a_type: auxv::AT_BASE, a_val: interp.base_addr as u64 });
    }

    let sp = setup_linux_stack(&mut stack, args, env, &auxv_vec);

    let hs = (loaded.brk + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    for i in 0..16 {
        let _ = loaded.address_space.alloc_and_map(hs + i * 0x1000, user_flags::RW_NO_EXEC);
    }

    if DEBUG_ELF_LOADING {
        log::debug!("[ELF] Heap pre-alloc: 0x{:x} (16 pages)", hs);
        log::debug!("[ELF] Stack: 0x{:x}-0x{:x}, SP=0x{:x}, argc={}",
            stack_bottom, stack_top, sp, args.len());
        if loaded.interp.is_some() {
            log::debug!("[ELF] Dynamic: start at interpreter 0x{:x}, AT_ENTRY=0x{:x}",
                actual_entry, loaded.entry_point);
        }
        log::debug!("[ELF] {} deferred lazy segments for demand paging",
            loaded.deferred_segments.len());
    }

    Ok((actual_entry, loaded.address_space, sp, hs, stack_bottom, stack_top, mmap_floor, loaded.deferred_segments))
}

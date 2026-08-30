//! Building the initial user stack: argv/envp/auxv per the Linux AArch64 ABI,
//! and the `load_elf_with_stack*` entry points `ProcessImage` calls.

use alloc::string::String;
use alloc::vec::Vec;

use akuma_mmap::{PAGE_SIZE, span, user_flags};
use akuma_mmu::UserAddressSpace;

use super::load::{LoadedElf, load_elf, load_elf_from_path};
use super::types::{AuxEntry, DEBUG_ELF_LOADING, DeferredLazySegment, ElfError, auxv};

/// What `load_elf_with_stack*` hands back: a loaded image plus the initial user
/// stack built for it.
///
/// A struct rather than the 8-tuple this used to be. Every consumer opened with
/// the same wide `let (a, b, c, d, e, f, g, h) = …` destructure, which is a large
/// part of why the four `ProcessImage` entry points were so hard to diff against
/// each other (Phase 2b).
pub struct LoadedWithStack {
    /// Where execution starts: the interpreter's entry point for a dynamically
    /// linked image, the image's own for a static one.
    pub entry_point: usize,
    pub address_space: UserAddressSpace,
    /// Initial SP, pointing at the argc/argv/envp/auxv block.
    pub sp: usize,
    /// Initial program break — the page-aligned end of the loaded image.
    pub brk: usize,
    pub stack_bottom: usize,
    pub stack_top: usize,
    /// Lowest VA `mmap` may hand out; non-zero only when an interpreter is loaded.
    pub mmap_floor: usize,
    /// Segments to register for demand paging. Empty unless the image was loaded
    /// with the deferred mapping strategy — see [`super::load::MapStrategy`].
    pub deferred_segments: Vec<DeferredLazySegment>,
}

/// Helper to build a userspace stack according to Linux AArch64 ABI
pub struct UserStack {
    pub stack_bottom: usize,
    pub stack_top: usize,
    pub sp: usize,
    pub frames: Vec<akuma_mmap::PhysFrame>,
}

impl UserStack {
    /// Copy `bytes` into the stack starting at virtual address `va`.
    ///
    /// Splits across frames via [`span::PageChunks`], so no write crosses a page.
    /// `&mut self` is deliberate and `needless_pass_by_ref_mut` stays allowed.
    /// The borrow checker sees only reads of `self.frames`, because the write
    /// goes through a *physical* alias of the stack's pages that it cannot track
    /// — but this call does mutate the stack's contents, and `&self` would let
    /// two shared borrows write the same frame concurrently. The signature is
    /// what makes the aliasing rule enforceable at the call sites.
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn write_at(&mut self, va: usize, bytes: &[u8]) {
        for c in span::PageChunks::new(self.stack_bottom, va, bytes.len()) {
            // `self.frames[..]` is bounds-checked, and `frame_write` asserts the
            // window stays inside the frame — between them, an arithmetic slip
            // panics instead of writing into whatever frame follows.
            crate::frame_write(
                self.frames[c.page_index].addr,
                c.offset,
                &bytes[c.src_offset..c.src_offset + c.len],
            );
        }
    }

    pub fn new(stack_bottom: usize, stack_top: usize, frames: Vec<akuma_mmap::PhysFrame>) -> Self {
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

        let sp = self.sp;
        self.write_at(sp, bytes);
        self.write_at(sp + bytes.len(), &[0u8]);
        self.sp
    }

    /// Push one native-endian `u64`.
    ///
    /// This used to assert by comment that "since SP was aligned to 8 or 16, a
    /// u64 won't cross a 4KB boundary" and then write through a single raw
    /// pointer. It no longer has to be true: `write_at` splits across pages if it
    /// ever is, so an alignment change upstream cannot silently corrupt the frame
    /// after this one.
    pub fn push_u64(&mut self, val: u64) {
        self.sp -= 8;
        let sp = self.sp;
        self.write_at(sp, &val.to_ne_bytes());
    }

    pub fn push_raw(&mut self, data: &[u8]) -> usize {
        self.sp -= data.len();

        let sp = self.sp;
        self.write_at(sp, data);
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
) -> Result<LoadedWithStack, ElfError> {
    attach_stack(load_elf(elf_data, interp_prefix)?, args, env, stack_size)
}

pub fn load_elf_with_stack_from_path(
    path: &str,
    file_size: usize,
    args: &[String],
    env: &[String],
    stack_size: usize,
    interp_prefix: Option<&str>,
) -> Result<LoadedWithStack, ElfError> {
    attach_stack(
        load_elf_from_path(path, file_size, interp_prefix)?,
        args,
        env,
        stack_size,
    )
}

/// Give a loaded image its initial stack, auxv and heap pre-allocation.
fn attach_stack(
    mut loaded: LoadedElf,
    args: &[String],
    env: &[String],
    stack_size: usize,
) -> Result<LoadedWithStack, ElfError> {
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
            .map_err(ElfError::MappingFailed)?;
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

    // `deferred_segments` is empty for an eagerly-mapped image and populated for
    // a demand-paged one. Passing it through unconditionally is what turns the
    // old hardcoded `Vec::new()` in the bytes variant into a property of the
    // mapping strategy — see `MapStrategy` in `load.rs`.
    Ok(LoadedWithStack {
        entry_point: actual_entry,
        address_space: loaded.address_space,
        sp,
        brk: hs,
        stack_bottom,
        stack_top,
        mmap_floor,
        deferred_segments: loaded.deferred_segments,
    })
}

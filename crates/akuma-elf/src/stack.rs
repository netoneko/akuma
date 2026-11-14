//! Building the initial user stack: argv/envp/auxv in the Linux layout, which
//! is the same on AArch64 and x86_64 (argc at SP, 16-byte aligned), and the
//! `load_elf_with_stack*` entry points `ProcessImage` calls. Both kernels build
//! their stacks here; [`attach_stack`] takes where the stack goes and the
//! `AT_HWCAP` word, which are the two things that differ.

use alloc::vec::Vec;

use akuma_mmap::{PAGE_SIZE, span};
use crate::pages::{SegProt, UserPages, alloc_page};

use super::load::{LoadedElf, load_elf, load_elf_from_path};
use super::types::{AuxEntry, DEBUG_ELF_LOADING, DeferredLazySegment, ElfError, auxv};

/// What `load_elf_with_stack*` hands back: a loaded image plus the initial user
/// stack built for it.
///
/// A struct rather than the 8-tuple this used to be. Every consumer opened with
/// the same wide `let (a, b, c, d, e, f, g, h) = …` destructure, which is a large
/// part of why the four `ProcessImage` entry points were so hard to diff against
/// each other (Phase 2b).
pub struct LoadedWithStack<A: UserPages> {
    /// Where execution starts: the interpreter's entry point for a dynamically
    /// linked image, the image's own for a static one.
    pub entry_point: usize,
    pub address_space: A,
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
/// Builds the initial user stack by writing through the address space that owns
/// its pages.
///
/// It used to hold `frames: Vec<PhysFrame>` and write through `phys_to_virt`
/// itself, which is what kept `unsafe` in this crate. Borrowing the
/// `UserAddressSpace` instead moves that write to
/// [`UserAddressSpace::write_page_bytes`], where `&mut self` on the address space
/// is a real exclusivity proof — a `PhysFrame` value is not, since
/// `PhysFrame::new` is safe and anyone can make one.
pub struct UserStack<'a, A: UserPages> {
    pub stack_bottom: usize,
    pub stack_top: usize,
    pub sp: usize,
    address_space: &'a mut A,
}

impl<'a, A: UserPages> UserStack<'a, A> {
    /// Copy `bytes` into the stack starting at virtual address `va`.
    ///
    /// Splits across frames via [`span::PageChunks`], so no write crosses a page.
    /// Copy `bytes` into the stack starting at virtual address `va`, splitting
    /// across pages via [`span::PageChunks`] so no write crosses a boundary.
    fn write_at(&mut self, va: usize, bytes: &[u8]) {
        let base = self.stack_bottom;
        for c in span::PageChunks::new(base, va, bytes.len()) {
            let page_va = base + c.page_index * PAGE_SIZE;
            let ok = self.address_space.write_page_bytes(
                page_va,
                c.offset,
                &bytes[c.src_offset..c.src_offset + c.len],
            );
            assert!(ok, "stack page {page_va:#x} is not mapped");
        }
    }

    /// The pages of `[stack_bottom, stack_top)` must already be mapped in
    /// `address_space` — [`write_at`](Self::write_at) asserts it.
    pub fn new(
        stack_bottom: usize,
        stack_top: usize,
        address_space: &'a mut A,
    ) -> Self {
        Self { stack_bottom, stack_top, sp: stack_top, address_space }
    }

    pub fn push_str(&mut self, s: impl AsRef<[u8]>) -> usize {
        let bytes = s.as_ref();
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

pub fn setup_linux_stack<A: UserPages, S: AsRef<[u8]>>(
    stack: &mut UserStack<'_, A>,
    args: &[S],
    env: &[S],
    auxv: &[AuxEntry],
) -> Result<usize, ElfError> {
    // Calculate total number of 8-byte words to be pushed
    // argc: 1
    // argv: args.len() + 1 (NULL)
    // envp: env.len() + 1 (NULL)
    // auxv: 2 * (auxv.len() + 1) (each entry is 2 words, + NULL entry)
    let total_words = 1 + (args.len() + 1) + (env.len() + 1) + 2 * (auxv.len() + 1);

    // Refuse before writing anything: the pushes below assert that each page
    // is mapped, and an argv larger than the stack would reach below it. The
    // strings, the two alignments (up to 15 bytes each), one padding word and
    // the word block.
    let strings = args.iter().chain(env).map(|a| a.as_ref().len() + 1).sum::<usize>();
    let needed = strings + 2 * 16 + 8 + total_words * 8;
    if needed > stack.sp - stack.stack_bottom {
        return Err(ElfError::MappingFailed("argv and envp do not fit the initial stack"));
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

    // Both ABIs want SP 16-byte aligned at entry, and SP points at argc, the
    // last of `total_words` words pushed below. So align here, after the
    // strings, and pad one word when the count is odd. The padding used to be
    // pushed before the strings, where this alignment discarded it: an odd
    // count entered the program with SP 8 bytes off (`auxvprobe`'s first
    // check, seen on amd64; AArch64's usual argv/envp counted even).
    stack.align_sp(16);
    if total_words % 2 != 0 {
        stack.push_u64(0);
    }

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

    Ok(stack.sp)
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

pub fn load_elf_with_stack<A: UserPages, S: AsRef<[u8]>>(
    elf_data: &[u8],
    args: &[S],
    env: &[S],
    stack_size: usize,
    interp_prefix: Option<&str>,
) -> Result<LoadedWithStack<A>, ElfError> {
    attach_stack(load_elf(elf_data, interp_prefix)?, args, env, None, stack_size, auxv::AARCH64_HWCAP)
}

pub fn load_elf_with_stack_from_path<A: UserPages, S: AsRef<[u8]>>(
    path: &str,
    file_size: usize,
    args: &[S],
    env: &[S],
    stack_size: usize,
    interp_prefix: Option<&str>,
) -> Result<LoadedWithStack<A>, ElfError> {
    attach_stack(
        load_elf_from_path(path, file_size, interp_prefix)?,
        args,
        env,
        None,
        stack_size,
        auxv::AARCH64_HWCAP,
    )
}

/// As [`load_elf_with_stack_from_path`], but every `PT_LOAD` is mapped now, read
/// off the file a window at a time, instead of registered as a lazy region.
///
/// For a kernel whose `#PF` handler does not serve `akuma-exec`'s lazy-region
/// table (amd64 pages from `Process::regions`). Heap use stays flat — the file is
/// never held whole — which is the other reason the path loader exists.
pub fn load_elf_with_stack_eager_from_path<A: UserPages, S: AsRef<[u8]>>(
    path: &str,
    args: &[S],
    env: &[S],
    stack_size: usize,
    interp_prefix: Option<&str>,
) -> Result<LoadedWithStack<A>, ElfError> {
    attach_stack(
        crate::load::load_elf_eager_from_path(path, interp_prefix)?,
        args,
        env,
        None,
        stack_size,
        auxv::AARCH64_HWCAP,
    )
}

/// Give a loaded image its initial stack and auxv.
///
/// `stack_top` is where the stack ends, or `None` for [`compute_stack_top`]'s
/// choice from the image's extent (AArch64). amd64 passes the top of the lower
/// half, where Linux puts it, because its `mmap` window runs up to the stack
/// and Go and Chromium hint addresses above 256 GiB. `stack_size` bytes below it
/// are mapped now. `hwcap` is the `AT_HWCAP` word, which is per architecture.
pub fn attach_stack<A: UserPages, S: AsRef<[u8]>>(
    mut loaded: LoadedElf<A>,
    args: &[S],
    env: &[S],
    stack_top: Option<usize>,
    stack_size: usize,
    hwcap: u64,
) -> Result<LoadedWithStack<A>, ElfError> {
    let has_interp = loaded.interp.is_some();
    let stack_top = stack_top.unwrap_or_else(|| compute_stack_top(loaded.brk, has_interp));
    let mmap_floor = if has_interp { 0x3010_0000 } else { 0 };
    let total_size = stack_size + PAGE_SIZE;
    let guard_page = (stack_top - total_size) & !(PAGE_SIZE - 1);
    let stack_bottom = guard_page + PAGE_SIZE;
    let stack_pages = (stack_size + PAGE_SIZE - 1) / PAGE_SIZE;

    for i in 0..stack_pages {
        let page_va = stack_bottom + i * PAGE_SIZE;
        alloc_page(&mut loaded.address_space, page_va, SegProt::Data)?;
    }

    let mut stack = UserStack::new(stack_bottom, stack_top, &mut loaded.address_space);
    // `AT_RANDOM`: musl and glibc seed the stack protector and pointer guard
    // from these bytes. A kernel with no RNG hook gives a fixed pattern rather
    // than zeros, which `canary == 0` checks would read as "uninitialised".
    let mut random = [0xA5u8; 16];
    if akuma_primitives::rng::fill_bytes(&mut random) != Some(true) {
        random = [0xA5; 16];
    }
    let random_ptr = stack.push_raw(&random);

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
    auxv_vec.push(AuxEntry { a_type: auxv::AT_SECURE, a_val: 0 });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_HWCAP, a_val: hwcap });
    auxv_vec.push(AuxEntry { a_type: auxv::AT_HWCAP2, a_val: 0 });
    // Linux passes `AT_BASE` always, 0 for a static image.
    auxv_vec.push(AuxEntry {
        a_type: auxv::AT_BASE,
        a_val: loaded.interp.as_ref().map_or(0, |i| i.base_addr as u64),
    });

    let sp = setup_linux_stack(&mut stack, args, env, &auxv_vec)?;

    // The break starts on a page boundary with nothing mapped above it:
    // `Process::set_brk` maps the heap and refuses to grow over a mapped page.
    let hs = (loaded.brk + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);

    if DEBUG_ELF_LOADING {
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

#[cfg(test)]
mod tests {
    //! `setup_linux_stack` against a page map, so the layout rules are checked
    //! without a kernel: SP alignment at every parity, the refusal of an argv
    //! that does not fit, and argc where SP points.
    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::vec;

    struct Pages(BTreeMap<usize, Vec<u8>>);

    impl UserPages for Pages {
        fn new_space() -> Option<Self> {
            Some(Self(BTreeMap::new()))
        }
        fn alloc_and_map(&mut self, va: usize, _prot: SegProt) -> Result<usize, &'static str> {
            self.0.insert(va, vec![0; PAGE_SIZE]);
            Ok(va)
        }
        fn write_page_bytes(&mut self, page_va: usize, offset: usize, bytes: &[u8]) -> bool {
            let Some(p) = self.0.get_mut(&page_va) else { return false };
            p[offset..offset + bytes.len()].copy_from_slice(bytes);
            true
        }
    }

    const TOP: usize = 0x10_0000;

    fn build(pages: usize, args: &[&[u8]], env: &[&[u8]], aux: usize) -> (Pages, Result<usize, ElfError>) {
        let mut space = Pages(BTreeMap::new());
        let bottom = TOP - pages * PAGE_SIZE;
        for i in 0..pages {
            space.alloc_and_map(bottom + i * PAGE_SIZE, SegProt::Data).unwrap();
        }
        let auxv: Vec<AuxEntry> = (0..aux).map(|i| AuxEntry { a_type: 100 + i as u64, a_val: 1 }).collect();
        let mut stack = UserStack::new(bottom, TOP, &mut space);
        // An odd start, as after `AT_RANDOM`'s 16 bytes and a string or two.
        stack.push_raw(&[0u8; 9]);
        let r = setup_linux_stack(&mut stack, args, env, &auxv);
        (space, r)
    }

    fn word(space: &Pages, va: usize) -> u64 {
        let page = va & !(PAGE_SIZE - 1);
        let off = va - page;
        u64::from_ne_bytes(space.0[&page][off..off + 8].try_into().unwrap())
    }

    #[test]
    fn sp_is_16_aligned_for_odd_and_even_word_counts() {
        // total_words = 1 + (a+1) + (e+1) + 2*(aux+1): parity follows a + e.
        for (a, e) in [(1usize, 0usize), (1, 1), (2, 1), (3, 4), (5, 7)] {
            let args: Vec<&[u8]> = (0..a).map(|_| b"arg".as_slice()).collect();
            let env: Vec<&[u8]> = (0..e).map(|_| b"K=V".as_slice()).collect();
            let (space, sp) = build(2, &args, &env, 5);
            let sp = sp.unwrap();
            assert_eq!(sp % 16, 0, "argc {a}, envc {e}");
            assert_eq!(word(&space, sp), a as u64, "argc is where SP points");
        }
    }

    #[test]
    fn an_argv_larger_than_the_stack_is_refused_not_written() {
        let big = vec![b'x'; 2 * PAGE_SIZE];
        let (_, r) = build(1, &[big.as_slice()], &[], 5);
        assert!(matches!(r, Err(ElfError::MappingFailed(m)) if m.contains("do not fit")));
    }
}

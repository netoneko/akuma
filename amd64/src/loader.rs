//! Stage L: giving a loaded ELF image its initial user stack.
//!
//! # What this file is now — C1 step 6
//!
//! It used to be a second ELF loader. It parsed the headers, placed the
//! `PT_LOAD` segments, read `PT_INTERP` and mapped the dynamic linker, and
//! *then* built the stack — 740 lines beside `crates/akuma-elf`, which does the
//! first three of those for the AArch64 kernel. The reason was structural and
//! it expired in two steps: B3 gave `akuma-mmu` an x86_64 `UserAddressSpace`,
//! and step 5a pointed this file at it, so by 2026-09-08 every signature here
//! already took the very type `akuma-elf` is written against
//! (`docs/archive/AKUMA_AMD64_STEP5A_ONE_WALKER.md`).
//!
//! The **loading** is `akuma-elf`'s now. What stayed is the **placing** of the
//! initial stack, and the split is drawn exactly there for a measured reason —
//! see "The layout, and why it did not move" below. `load` is a thin adapter:
//! it calls [`akuma_elf::load_elf`], which builds the address space, maps every
//! `PT_LOAD`, and loads the interpreter if the image names one, and it turns
//! that crate's `LoadedElf` into the [`LoadedImage`] this target's `build_stack`
//! and boot checks already read.
//!
//! # The layout, and why it did not move
//!
//! The two loaders place a program in *almost* the same places, and the one
//! difference that matters is not the interpreter's:
//!
//! | | this target, before | `akuma-elf` |
//! |---|---|---|
//! | `PIE_BASE` | `0x1000_0000` | `0x1000_0000` — the same |
//! | `INTERP_BASE` | `0x4000_0000` | `0x3000_0000` |
//! | stack top | [`crate::usermode::ELF_STACK_TOP`], fixed | `compute_stack_top(brk, has_interp)`, variable, capped at `0x40_0000_0000` |
//! | mmap window | `mm::MMAP_BASE` … `mm::MMAP_TOP` (112 TiB) | `mmap_floor` = `0x3010_0000` |
//!
//! **The interpreter base moved and nothing else did.** `0x3000_0000` sits in
//! the same hole `0x4000_0000` did — above a static-PIE program at `PIE_BASE`
//! and 3.75 GiB below `mm::MMAP_BASE` (`0x1_0000_0000`) — so an image and its
//! mappings still cannot meet. That is an explicitly carried decision, not an
//! accident: taking `akuma-elf`'s loading means taking where it puts the linker,
//! and a program would have to be 512 MiB to reach it from `PIE_BASE`.
//!
//! `akuma-elf`'s **stack** placement is the one that could not come along, and
//! this is measured rather than predicted. `compute_stack_top` caps at
//! `0x40_0000_0000` (256 GiB), which is *inside* `mm.rs`'s window
//! `[0x1_0000_0000, 0x7000_0000_0000)`. The stack is not in the region list —
//! it is placed here — and `MMAP_TOP` was chosen at 112 TiB precisely so the
//! fixed [`crate::usermode::ELF_STACK_TOP`] sits outside the window "by
//! construction rather than by collision test" (`mm.rs`'s own comment). An
//! `akuma-elf`-placed stack breaks that construction: `find_free_va` would hand
//! out the stack's own pages, and the failure is a `SIGSEGV` in ring 3 with no
//! message. So `build_stack` below stays, and `attach_stack`/`compute_stack_top`
//! are deliberately not called.
//!
//! # Divergences this fold carries, each one deliberate
//!
//! Refusals the old `place_image` made that `akuma-elf` does not:
//!
//! * **A segment outside the lower half** is no longer refused *here*. It is
//!   refused one level down, in `akuma_mmu`'s x86 walk, which is where it
//!   belonged all along: `UserAddressSpace::new` aliases the kernel's PML4
//!   slots into every user root, so an upper-half `p_vaddr` was never one
//!   process's problem — see `x86_map_page_in`'s header and
//!   `uas::upper_half_refusal_test`.
//! * **A writable+executable segment** was refused outright; `akuma-elf`
//!   enforces W^X by construction instead (`SegProt` has two variants and
//!   `PF_X` wins), so such a segment loads as read-execute and faults on its
//!   first write rather than failing the load. Strictly safer, less legible.
//!   `userspace/amd64/user.ld` page-aligns every segment so neither answer is
//!   reachable from our own image.
//! * **`p_filesz > p_memsz`** was refused; `akuma-elf` copies only what fits in
//!   the `memsz` page span, silently truncating. Not dangerous — the bytes go
//!   nowhere — but it is a refusal that became a shrug.
//! * **The entry point** was checked for being a non-zero user address inside an
//!   executable segment. That one is cheap to keep without a second parse and
//!   [`load`] keeps it, reading the permission back out of the page tables.
//!
//! Gaps `akuma-elf` closes for free, which would otherwise have vanished:
//!
//! * A `PT_INTERP` of one NUL byte — what a static-PIE emits — was read as an
//!   empty path and turned into a failed `read_file`. `akuma-elf` skips any
//!   `PT_INTERP` with `p_filesz <= 1`.
//! * `PN_XNUM` and a bad `e_phentsize` are refused in `parse_headers` rather
//!   than here, and every header field is read through the bounds-checked
//!   `elf` 0.7 crate rather than by this file re-doing the same reads.
//!
//! What did **not** get closed, and stays a bound on this target: `build_stack`
//! assembles the word block in a `[u8; STACK_WORDS_MAX * 8]` on the kernel
//! stack, so [`MAX_ARGV`] and [`MAX_ENVP`] are still hard caps. `akuma-elf`'s
//! `setup_linux_stack` builds on the heap and has no such limit — that is a gap
//! it *would* close, and taking it means taking its auxv (fourteen entries
//! against this target's seven) and its stack placement, which is the layout
//! question above. Recorded as owed rather than quietly kept.
//!
//! # What ring 3 gets on its stack
//!
//! Unchanged by the fold. `_start` receives no arguments, so this block is how a
//! program learns its own name, its environment and the page size — and how a
//! static-PIE finds its own program headers to self-relocate against
//! (`AT_PHDR`/`AT_PHNUM`/`AT_PHENT`), and how a dynamic linker finds its own
//! (`AT_BASE`). Omit `AT_BASE` and `ld-musl` self-relocates against address 0
//! and faults in the first page before it runs a line of the program.

use akuma_mmap::PhysFrame;

use akuma_elf::ElfError;
use akuma_mmu::{PteProt, UserAddressSpace};

use crate::phys::phys_ptr;

const PAGE_SIZE: usize = 4096;

/// First address that is not userspace: PML4 slot 256 and up is the kernel's.
///
/// Only [`load`]'s entry-point check reads it now. The *segment* check that
/// used to is `akuma_mmu::USER_HALF_END`, enforced inside the walk.
const USER_VA_LIMIT: u64 = 0x0000_8000_0000_0000;

/// Auxiliary-vector keys this kernel supplies.
///
/// Spelled here rather than pulled from `akuma_elf::types::auxv`, which is
/// `pub` but names eleven keys this target does not supply — a `use` of the
/// module would read as though it did.
const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_BASE: u64 = 7;
const AT_ENTRY: u64 = 9;
const AT_UID: u64 = 11;
const AT_EUID: u64 = 12;
const AT_GID: u64 = 13;
const AT_EGID: u64 = 14;
const AT_HWCAP: u64 = 16;
const AT_CLKTCK: u64 = 17;
const AT_SECURE: u64 = 23;
const AT_RANDOM: u64 = 25;

/// What a successful load produced.
///
/// `akuma_elf::LoadedElf` in this target's vocabulary. The two differ in three
/// places and each is a real translation rather than a rename: [`Self::entry`]
/// is where ring 3 is entered (the *linker's* entry for a dynamic image, which
/// `LoadedElf` keeps in `interp`), [`Self::interp_base`] flattens
/// `Option<InterpInfo>` to the `0` Linux reports for a static image, and
/// [`Self::end_va`] is page-aligned where `brk` is not.
pub struct LoadedImage {
    /// Where `enter_user_mode` jumps. `base + e_entry` for a static image, and
    /// the **dynamic linker's** entry for one with a `PT_INTERP` — the linker
    /// brings the program up and jumps to it itself.
    pub entry: u64,
    /// The program's own entry point, for `AT_ENTRY`. Equal to [`Self::entry`]
    /// for a static image; for a dynamic one it is what the linker jumps to
    /// when it is finished, so reporting the linker's own here would loop.
    pub prog_entry: u64,
    /// Where the dynamic linker was placed, for `AT_BASE`. `0` for a static
    /// image, which is also what Linux reports there.
    ///
    /// Not decoration: a PIE interpreter is linked at 0 and has no other way to
    /// find its own relocations. Omit it and `ld-musl` self-relocates against
    /// address 0 and faults in the first page before it runs a line of the
    /// program.
    pub interp_base: u64,
    /// Page-aligned end of the highest `PT_LOAD`. Where a `brk` heap would
    /// start.
    pub end_va: u64,
    /// Where the program header table ended up in the mapped image, for
    /// `AT_PHDR`. `0` when the image carries no `PT_PHDR` and no `PT_LOAD`
    /// beginning at file offset 0 — the hand-linked `userspace/amd64/hello`
    /// and `fdprobe` probes are both in that shape and neither reads its own
    /// auxv, so it is a fallback rather than a load failure.
    pub phdr_addr: u64,
    /// `e_phnum`, for `AT_PHNUM`.
    pub phnum: u16,
    /// `e_phentsize`, for `AT_PHENT` — always 56 on ELF64 (checked in
    /// `akuma_elf`'s `parse_headers`), but passed through rather than
    /// hard-coded a second place.
    pub phent: u16,
}

/// Load `image` into a fresh address space through [`akuma_elf::load_elf`].
///
/// # Why this returns the address space rather than filling one
///
/// `load_elf` builds it: `A::new_space()` is the first thing it does, because
/// the image's own headers decide how many page tables it needs and the loader
/// is the only thing that has read them. The old signature took `&mut
/// UserAddressSpace` because it was the *caller* that made one. On failure the
/// space is dropped inside the crate and its destructor returns every frame the
/// half-finished load had taken — which is what `elf: rejected loads leak
/// nothing` checks on real hardware.
///
/// # The eager strategy, and why not the deferred one
///
/// [`akuma_elf::load_elf`] maps every page of every `PT_LOAD` up front.
/// `load_elf_from_path` would register demand-paged lazy regions instead, and
/// this target cannot use it: its two file-reading hooks (`read_at`,
/// `resolve_file_id`) are `exec_runtime.rs` category-3 stubs that panic naming
/// themselves, and its `#PF` handler pages from `Process::regions`
/// (`akuma-mmap`) rather than from `akuma-exec`'s lazy-region table. Both are
/// C2's to fold.
pub fn load(image: &[u8]) -> Result<(UserAddressSpace, LoadedImage), &'static str> {
    let loaded = akuma_elf::load_elf::<UserAddressSpace>(image, None).map_err(|e| elf_err(&e))?;

    let entry = match loaded.interp {
        // Ring 3 is entered in the linker, which brings the program up itself.
        Some(ref interp) => interp.entry_point as u64,
        None => loaded.entry_point as u64,
    };
    // `AT_ENTRY` stays the *program's* entry: it is what the linker jumps to
    // once it is done, and reporting the linker's own would loop.
    let prog_entry = loaded.entry_point as u64;

    // The three refusals `place_image` made after placing an image, kept
    // because they cost no second parse — the answers are read back out of the
    // page tables the loader just wrote, which is what the hardware will do
    // rather than what the loader believes it did.
    if prog_entry == 0 || prog_entry >= USER_VA_LIMIT {
        return Err("entry point is not a user address");
    }
    if entry == 0 || entry >= USER_VA_LIMIT {
        return Err("interpreter entry point is not a user address");
    }
    if loaded
        .address_space
        .pte_prot(entry as usize & !(PAGE_SIZE - 1))
        .is_none_or(|(p, _cow)| !p.exec)
    {
        return Err("entry point is not in an executable segment");
    }

    let img = LoadedImage {
        entry,
        prog_entry,
        interp_base: loaded.interp.as_ref().map_or(0, |i| i.base_addr as u64),
        // The heap starts past the program, not past the linker: `brk` grows up
        // from the program image and the linker sits above it either way.
        // `LoadedElf::brk` is the highest `p_vaddr + p_memsz`, unrounded.
        end_va: align_up(loaded.brk as u64, PAGE_SIZE as u64),
        phdr_addr: loaded.phdr_addr as u64,
        phnum: loaded.phnum as u16,
        phent: loaded.phent as u16,
    };
    Ok((loaded.address_space, img))
}

/// [`ElfError`] as one of this module's `&'static str`s.
///
/// A match rather than `Display`: the caller's error channel is a `&'static
/// str` all the way up to the boot suite and `execve`'s errno mapping, and
/// rendering through `format!` to get one would be a heap allocation on the
/// path that is reporting a failure. The arms that carry their own `&'static
/// str` pass it through, so a `MappingFailed("Out of memory for user page")`
/// still says which resource ran out.
fn elf_err(e: &ElfError) -> &'static str {
    match *e {
        ElfError::InvalidFormat(m) | ElfError::MappingFailed(m) => m,
        ElfError::InvalidMagic(_) => "not an ELF image",
        ElfError::WrongArchitecture => "not an x86-64 image",
        ElfError::NotExecutable => "not ET_EXEC or ET_DYN",
        ElfError::DynamicallyLinked => "dynamically linked and no interpreter could be loaded",
        ElfError::OutOfMemory => "out of memory loading an image",
        ElfError::AddressSpaceFailed => "no frame for a PML4",
    }
}

/// Round `v` up to the next multiple of `to`.
const fn align_up(v: u64, to: u64) -> u64 {
    v.div_ceil(to) * to
}

/// The permissions a page needs to satisfy both `a` and `b`.
///
/// Reached only from [`map_range`]'s already-mapped arm, which since C1 step 6
/// nothing can take: `map_range`'s one caller is [`build_stack`], and the stack
/// sits at [`crate::usermode::ELF_STACK_TOP`] — 128 TiB above anything an image
/// occupies. Kept, rather than deleted with the segment placer it was written
/// for, because it is what makes `map_range` safe to point at a second range
/// later; the W^X refusal below is the property worth keeping alive.
const fn widen(a: PteProt, b: PteProt) -> PteProt {
    PteProt { write: a.write || b.write, exec: a.exec || b.exec, user: a.user || b.user }
}

/// The CoW marker every mapping this module writes carries: **none**.
///
/// A freshly loaded image shares nothing — `fork` is what marks pages — so every
/// `map_*_pte` call below passes `false`. Named rather than repeated as a bare
/// literal at eight call sites, where a stray `true` would be a page that faults
/// on its first write and gets silently copied.
const NOT_COW: bool = false;

/// Copy `src` into `space` at virtual address `va`, page by page.
///
/// Writes through the physmap rather than through `va` itself: the address
/// space being filled is not the active one — that is the whole point of
/// building it before `CR3` ever names it — so its virtual addresses mean
/// nothing to the CPU right now.
fn write_user(space: &UserAddressSpace, va: u64, src: &[u8]) -> bool {
    let mut done = 0usize;
    while done < src.len() {
        let at = va + done as u64;
        let page = at & !(PAGE_SIZE as u64 - 1);
        let off = (at - page) as usize;
        let n = (PAGE_SIZE - off).min(src.len() - done);
        let Some(pa) = space.translate(page as usize) else {
            return false;
        };
        // SAFETY: `pa` came from a walk of this space's own tables, so it is a
        // live frame; the physmap makes it dereferenceable, and `off + n` is
        // bounded by PAGE_SIZE by construction above.
        unsafe {
            core::ptr::copy_nonoverlapping(
                src.as_ptr().add(done),
                phys_ptr::<u8>(pa as u64).add(off),
                n,
            );
        }
        done += n;
    }
    true
}

#[cfg(not(feature = "no-tests"))]
/// Read `dst.len()` bytes out of `space` at `va`, through the physmap.
///
/// The counterpart of [`write_user`], and test-only: nothing in the running
/// kernel reads a *foreign* address space this way — a live one is read through
/// `uaccess` with `CR3` already naming it. The self-test that checks what the
/// stack builder actually wrote has no such luxury, since the space it is
/// inspecting has never been entered.
pub fn read_user(space: &UserAddressSpace, va: u64, dst: &mut [u8]) -> bool {
    let mut done = 0usize;
    while done < dst.len() {
        let at = va + done as u64;
        let page = at & !(PAGE_SIZE as u64 - 1);
        let off = (at - page) as usize;
        let n = (PAGE_SIZE - off).min(dst.len() - done);
        let Some(pa) = space.translate(page as usize) else {
            return false;
        };
        // SAFETY: as `write_user`, in the other direction — `pa` came from a
        // walk of this space's own tables and `off + n` is bounded by PAGE_SIZE.
        unsafe {
            core::ptr::copy_nonoverlapping(
                phys_ptr::<u8>(pa as u64).add(off),
                dst.as_mut_ptr().add(done),
                n,
            );
        }
        done += n;
    }
    true
}

/// Make sure every page of `[start, end)` is mapped in `space` with at least
/// `prot`, allocating and zeroing frames as needed.
///
/// One caller since C1 step 6: [`build_stack`]. Zeroing on allocation is what
/// stops a recycled frame handing ring 3 whatever the previous owner left in
/// it — for a stack that is the whole of its job, and the same rule the
/// demand-paging handler follows. (It used to also implement `p_memsz >
/// p_filesz` for the segment placer; `akuma-elf` discharges that obligation
/// through `UserPages::alloc_and_map`, whose contract says the page arrives
/// zeroed for exactly this reason.)
fn map_range(
    space: &mut UserAddressSpace,
    start: u64,
    end: u64,
    prot: PteProt,
) -> Result<(), &'static str> {
    let mut va = start;
    while va < end {
        if let Some((existing, _cow)) = space.pte_prot(va as usize) {
            // A page a previous segment already placed. Only reachable from an
            // unaligned link; see `widen`.
            let want = widen(existing, prot);
            if want.write && want.exec {
                return Err("segments share a page and would make it writable+executable");
            }
            if want != existing {
                let pa = space.translate(va as usize).ok_or("mapped page has no frame")?;
                if !space.map_page_pte(va as usize, pa, want, NOT_COW) {
                    return Err("could not widen a shared page's permissions");
                }
            }
        } else {
            let pa = akuma_pmm::alloc_page().ok_or("out of frames loading a segment")? as u64;
            // SAFETY: a fresh PMM frame, reached through the physmap.
            unsafe { core::ptr::write_bytes(phys_ptr::<u8>(pa), 0, PAGE_SIZE) };
            // `map_and_track_pte` records the frame **before** it maps, and
            // untracks it again if the map fails — the obligation this used to
            // discharge with a bare `track_user_frame` above the `map`. A frame
            // the ledger does not know about is a frame teardown will not
            // release; a frame it knows about but nothing maps is one this
            // address space would free out from under its next owner.
            if !space.map_and_track_pte(va as usize, PhysFrame::new(pa as usize), prot, NOT_COW) {
                return Err("could not map a segment page");
            }
        }
        va += PAGE_SIZE as u64;
    }
    Ok(())
}

/// Map a stack below `top` and lay out the System V initial frame on it.
///
/// Returns the value to put in `rsp` before entering ring 3.
///
/// # The layout, which the program is compiled against
///
/// ```text
///   rsp -> argc
///          argv[0] .. argv[argc-1]
///          NULL
///          envp[0] .. NULL
///          auxv key/value pairs, terminated by AT_NULL
///          ...
///   top -> argv[0]'s string bytes
/// ```
///
/// This is not decoration: `_start` receives no arguments, so *this block* is
/// how a program learns its own name, its environment and the page size. A
/// kernel that maps a stack and sets `rsp` without building it has produced a
/// program that runs and reads garbage — which is why `hello.rs` checks three of
/// these fields and reports them in its exit status.
/// The most argv entries the initial stack builder will place.
///
/// **Was 16 until 2026-09-12, and that cost a week of the wrong suspicion.**
/// A shell invoked as `sh -c "<cmd>"` needs three, which is what the original
/// bound was sized for; a *toolchain* does not. `rustc`'s call to its linker
/// passes about fifty arguments and `collect2`'s call to `ld` a similar number,
/// and the sixteenth onward were silently dropped — `sys_execve` truncated
/// rather than refusing. The linker therefore ran with a command line that was
/// valid, shorter, and missing its `-o` and its input files, so it reported
/// "no input files" and `cannot open output file a.out`, and every
/// investigation went looking at `collect2` and at path resolution
/// (`docs/archive/RUST_TOOLCHAIN_AMD64.md`, "Open after session 2", item 1).
///
/// The probe that settles it in one line, in the guest:
/// `busybox echo a b c … z` prints exactly fourteen letters — sixteen argv
/// entries counting `busybox` and `echo`.
///
/// 256 covers a linker invocation with a large crate graph. The cost is the
/// word block below, which is `STACK_WORDS_MAX * 8` bytes of **kernel** stack
/// against `sched::STACK_SIZE`; the string pointers now live in that same
/// block rather than in arrays beside it, which is what keeps the growth to
/// ~2 KiB and not ~5.
pub const MAX_ARGV: usize = 256;

/// The most envp entries the initial stack builder will place. `execve` from a
/// shell hands the child its whole environment; 64 covers a login shell's
/// `PATH`/`HOME`/`TERM`/… and the dozen `CARGO_*`/`RUST*` variables a build
/// adds, and bounds the copy (same reasoning as [`MAX_ARGV`]).
pub const MAX_ENVP: usize = 64;

/// Words in the fixed word block: argc, argv ptrs + NULL, envp ptrs + NULL,
/// and the auxv — **fifteen** key/value pairs, so thirty words: `AT_PHDR`,
/// `AT_PHENT`, `AT_PHNUM`, `AT_PAGESZ`, `AT_ENTRY`, `AT_BASE`, `AT_UID`,
/// `AT_EUID`, `AT_GID`, `AT_EGID`, `AT_HWCAP`, `AT_CLKTCK`, `AT_SECURE`,
/// `AT_RANDOM` and `AT_NULL`.
///
/// This must be kept in step with the `words` computation in [`build_stack`],
/// which writes into a `[u8; STACK_WORDS_MAX * 8]`. It is the *bound*, not the
/// count — the assertion below is what makes a drift a build failure instead of
/// an index-out-of-bounds panic in the kernel on the first program with a full
/// argv. `AT_BASE` was added on 2026-09-06 and this constant was **not** bumped
/// with it, which is exactly the shape of bug the assertion now prevents.
/// The eight glibc-facing pairs were added 2026-09-12: glibc's `ld.so`
/// segfaulted on its first relocation against address 0 with the seven-pair
/// vector (musl never reads the difference), and `docs/archive/` has no record
/// of which entry it wanted — so all the identity/entropy entries Linux
/// supplies came in at once rather than one guess per reboot.
const AUXV_WORDS: usize = 30;
const STACK_WORDS_MAX: usize = 1 + (MAX_ARGV + 1) + (MAX_ENVP + 1) + AUXV_WORDS;

/// As [`load`]'s return value: `AT_PHDR`/`AT_PHNUM`/`AT_PHENT` are what let a
/// static-PIE binary (`apk`) find its own program headers and self-relocate
/// — see the module header's "Static-PIE" section. An `ET_EXEC` image (every
/// other program on this target) ignores them; they cost three more auxv
/// words and nothing else.
pub fn build_stack(
    space: &mut UserAddressSpace,
    top: u64,
    pages: usize,
    argv: &[&[u8]],
    envp: &[&[u8]],
    img: &LoadedImage,
) -> Result<u64, &'static str> {
    if !top.is_multiple_of(PAGE_SIZE as u64) || pages == 0 {
        return Err("stack top must be page aligned and at least one page");
    }
    if argv.is_empty() || argv.len() > MAX_ARGV {
        return Err("argv must hold 1..=MAX_ARGV entries");
    }
    if envp.len() > MAX_ENVP {
        return Err("envp exceeds MAX_ENVP entries");
    }
    let bytes = (pages as u64) * PAGE_SIZE as u64;
    let base = top.checked_sub(bytes).ok_or("stack underflows the address space")?;
    map_range(space, base, top, PteProt::USER_RW)?;

    // The argv then envp strings sit at the very top, NUL-terminated, packed
    // downward. Where each string's bytes land is also the pointer the program
    // reads, so the cursor is walked once here to find the bottom of the blob
    // and again below to fill in the pointers — rather than kept in a
    // `[u64; MAX_ARGV]` beside the word block, which at a 256-entry argv is two
    // kilobytes of kernel stack holding what the word block is about to hold
    // anyway.
    let mut cursor = top;
    for s in argv.iter().chain(envp) {
        cursor -= s.len() as u64 + 1;
    }
    // `AT_RANDOM` points at 16 bytes on the stack, below the string blob.
    // glibc's `ld.so` reads it unconditionally for its pointer-guard setup;
    // handing it 0 makes every guard 0, which is worse than absent. musl and
    // every probe on this target ignore it.
    cursor -= 16;
    let random_va = cursor;
    let mut random = [0u8; 16];
    let seeded = akuma_primitives::rng::fill_bytes(&mut random);
    if seeded != Some(true) {
        // Fall back to a fixed but non-zero pattern rather than leaving zeros:
        // a 0 guard is the one value a relative-pointer forge trivially beats.
        random = [0xA5; 16];
    }
    // Round the whole string blob down to 16 so the word block below starts
    // aligned without a second adjustment.
    let strings_base = cursor & !0xf;

    // argc, one pointer per argv entry, argv NULL, one per envp entry, envp
    // NULL, fifteen auxv pairs — see `AUXV_WORDS`.
    let words = 1 + argv.len() + 1 + envp.len() + 1 + AUXV_WORDS;
    debug_assert!(
        words <= STACK_WORDS_MAX,
        "the auxv grew past what STACK_WORDS_MAX budgets"
    );
    let rsp = (strings_base - (words as u64) * 8) & !0xf;
    if rsp < base {
        return Err("initial stack frame does not fit");
    }

    // Assemble the word block on the kernel stack, then copy it in one shot.
    let mut buf = [0u8; STACK_WORDS_MAX * 8];
    let mut put = |slot: usize, v: u64| {
        buf[slot * 8..slot * 8 + 8].copy_from_slice(&v.to_le_bytes());
    };
    put(0, argv.len() as u64);
    // Second walk of the same cursor: `argv[i]` is the address its bytes get,
    // and the slot it goes in is the one the program will read it from.
    let mut sp = top;
    for (i, a) in argv.iter().enumerate() {
        sp -= a.len() as u64 + 1;
        put(1 + i, sp);
    }
    put(1 + argv.len(), 0); // argv terminator
    for (i, e) in envp.iter().enumerate() {
        sp -= e.len() as u64 + 1;
        put(2 + argv.len() + i, sp);
    }
    put(2 + argv.len() + envp.len(), 0); // envp terminator
    debug_assert_eq!(sp, cursor + 16, "both cursor walks must land together");
    let aux = 3 + argv.len() + envp.len();
    put(aux, AT_PHDR);
    put(aux + 1, img.phdr_addr);
    put(aux + 2, AT_PHENT);
    put(aux + 3, u64::from(img.phent));
    put(aux + 4, AT_PHNUM);
    put(aux + 5, u64::from(img.phnum));
    put(aux + 6, AT_PAGESZ);
    put(aux + 7, PAGE_SIZE as u64);
    put(aux + 8, AT_ENTRY);
    // The *program's* entry, which for a dynamic image is not where ring 3 is
    // entered. See `LoadedImage::prog_entry`.
    put(aux + 9, img.prog_entry);
    put(aux + 10, AT_BASE);
    put(aux + 11, img.interp_base);
    put(aux + 12, AT_UID);
    put(aux + 13, 0);
    put(aux + 14, AT_EUID);
    put(aux + 15, 0);
    put(aux + 16, AT_GID);
    put(aux + 17, 0);
    put(aux + 18, AT_EGID);
    put(aux + 19, 0);
    put(aux + 20, AT_HWCAP);
    // Deliberately 0: baseline x86-64 only. A bit set here licenses ifunc
    // resolvers (AVX `memcpy` and friends) whose dispatch glibc assumes the
    // kernel checked — claim nothing and it picks the baseline copies.
    put(aux + 21, 0);
    put(aux + 22, AT_CLKTCK);
    put(aux + 23, 100);
    put(aux + 24, AT_SECURE);
    put(aux + 25, 0);
    put(aux + 26, AT_RANDOM);
    put(aux + 27, random_va);
    put(aux + 28, AT_NULL);
    put(aux + 29, 0);

    // The strings themselves, read back from the slots just filled so there is
    // one source of truth for where each one goes.
    let get = |slot: usize| -> u64 {
        let mut w = [0u8; 8];
        w.copy_from_slice(&buf[slot * 8..slot * 8 + 8]);
        u64::from_le_bytes(w)
    };
    for (i, s) in argv.iter().enumerate().chain(
        envp.iter().enumerate().map(|(i, e)| (i + argv.len() + 1, e)),
    ) {
        let va = get(1 + i);
        if !write_user(space, va, s) || !write_user(space, va + s.len() as u64, &[0]) {
            return Err("could not write argv/envp");
        }
    }
    if !write_user(space, random_va, &random) {
        return Err("could not write the AT_RANDOM bytes");
    }
    if !write_user(space, rsp, &buf[..words * 8]) {
        return Err("could not write the initial stack frame");
    }
    Ok(rsp)
}

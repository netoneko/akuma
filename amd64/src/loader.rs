//! Stage L: loading a real ELF image into a process address space.
//!
//! Everything before this stage ran a program the kernel assembled for itself,
//! byte by byte, in `usermode::build_user_program`. That was honest about being
//! a placeholder: it proved ring 3, syscalls and preemption without needing a
//! loader, and it could only ever run code this file knows how to emit.
//!
//! What runs now is `userspace/amd64/hello/hello.rs`, compiled by `rustc` and
//! linked by `userspace/amd64/user.ld` at `0x40_0000`, embedded in the kernel
//! image and **parsed at boot**. The blocker was Stage K, not this file: until the kernel
//! left the lower half there was nowhere to put a program linked where a static
//! Linux binary is linked, and a loader that can only place an image at an
//! address chosen to dodge the kernel is a loader for one program.
//!
//! # Why this is not `akuma-elf`
//!
//! The tree has an ELF loader — `crates/akuma-elf` — and it is arch-neutral in
//! everything that matters here: `source.rs` parses through the vetted `elf`
//! 0.7 crate and never names an architecture. It is unusable from this target
//! anyway, for one structural reason: `load.rs`, `interp.rs` and `stack.rs` are
//! all written against `akuma_mmu::UserAddressSpace`, and `akuma-mmu` is
//! AArch64 page-table code. The dependency is real, not incidental — a loader
//! that cannot name an address space cannot place a segment, which is the
//! reasoning recorded in that crate's own manifest.
//!
//! So the split that would let the two share is a **parse/place split**: the
//! `ElfSource` + `parse_headers` half is neutral and `pub(super)`, the mapping
//! half is not. That is the shape a future extraction should take, and it is
//! not this stage's work. What this file deliberately does *not* do is
//! re-implement the parsing: it calls the same `elf` 0.7 crate through the same
//! `parse_ident` / `parse_tail` / `SegmentTable` path, so the tree still has one
//! ELF parser and two consumers of it — not two parsers, which is the defect
//! `docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §3 spent a verification
//! campaign removing.
//!
//! # What it refuses
//!
//! * Anything but `ET_EXEC`/`ET_DYN` + `EM_X86_64` + ELF64 little-endian.
//! * A `PT_INTERP` segment — a *dynamically*-linked binary (one that names
//!   `/lib/ld-musl-x86_64.so.1` and expects the kernel or a separate loader
//!   run to satisfy symbol imports against it) still has no home here. What
//!   `ET_DYN` alone buys (since 2026-09-04, for `apk`) is **static-PIE**: a
//!   fully self-contained image that only needs a base address and its own
//!   `_start` to bring itself up — see "Static-PIE" below.
//! * A segment outside the lower half. The upper half is the kernel's, and a
//!   `p_vaddr` there would ask `map_page_in` to overwrite a shared PML4 entry.
//! * A page that would end up **writable and executable**. `Prot` offers no
//!   `USER_RWX` constructor for the same reason, and this is where that becomes
//!   enforcement rather than convention: an unaligned link packs .text and .data
//!   into one page, and the union of their permissions is W+X.
//!   `userspace/amd64/user.ld` aligns every segment to a page precisely so this
//!   refusal never fires on our own image — a link that stops satisfying it
//!   fails the boot instead of silently handing ring 3 a writable code page.
//!
//! # Static-PIE (`ET_DYN`, no `PT_INTERP`)
//!
//! Alpine's `apk-tools-static` — the reason this exists — is compiled
//! `-static-pie`: `ET_DYN`, no `PT_INTERP`, and its relocations are `DT_RELR`
//! (a compact bitmap format, not a classic `SHT_RELA` array — confirmed by
//! reading its own `.dynamic` section, `RELASZ` is 0 and `RELR`/`RELRSZ` carry
//! everything). This loader does **not** process them. It does not need to:
//! musl's own startup (`_dlstart_c`) walks its *own* program headers — found
//! through `AT_PHDR` in the auxv this file now supplies — computes its load
//! bias from where the kernel actually put them versus where they claim to be
//! linked at (0, for a PIE), and self-relocates before calling `main`. The
//! kernel's entire job is: pick a base (`PIE_BASE`), map every `PT_LOAD` at
//! `base + p_vaddr` instead of `p_vaddr`, and report `AT_PHDR`/`AT_PHNUM`/
//! `AT_PHENT` truthfully. Everything past that is the binary's own problem,
//! by design — the same division of labor `userspace/apk-tools/docs/
//! PIE_LOADER.md` documents for the AArch64 kernel, which this mirrors.
//!
//! One consequence worth stating: the data segment RELR relocations land in
//! must be **writable** at load time for `_dlstart_c` to write into it, and
//! this loader never re-protects it read-only afterward (`PT_GNU_RELRO` is
//! parsed nowhere here) — a real security regression from what a hardened
//! loader would do, accepted for the same reason the rest of this file's
//! "what it does not do yet" list is accepted.
//!
//! # What it does not do yet
//!
//! No demand paging: every page of every `PT_LOAD` is allocated and copied up
//! front. The `#PF` handler can already service a not-present fault
//! (`idt.rs`), so the machinery exists; wiring segments to it needs a per-space
//! region table, which is the next thing rather than part of this one. No real
//! dynamic linker (`PT_INTERP` is refused outright), and no `PT_GNU_RELRO`.

use elf::abi::{EM_X86_64, ET_DYN, ET_EXEC, PF_W, PF_X, PT_INTERP, PT_LOAD};
use elf::endian::LittleEndian;
use elf::file::{Class, FileHeader};
use elf::segment::SegmentTable;

use crate::paging::{AddressSpace, MemAttr, Prot};
use crate::phys::phys_ptr;

const PAGE_SIZE: usize = 4096;
/// Bytes of `e_ident`, and the offset the rest of the header starts at.
const EI_NIDENT: usize = 16;
/// Size of an ELF64 file header.
const ELF64_EHDR_SIZE: usize = 64;

/// First address that is not userspace: PML4 slot 256 and up is the kernel's.
const USER_VA_LIMIT: u64 = 0x0000_8000_0000_0000;

/// Where a static-PIE (`ET_DYN`) image's segments are placed. `p_vaddr`
/// values in a PIE start near 0 (it is linked as if loaded at address 0), so
/// this is added to every one of them — the same constant, and the same
/// reasoning, as `akuma-elf`'s `PIE_BASE` for the AArch64 kernel: well above
/// where an `ET_EXEC` image links (`0x40_0000`) and, on this target, well
/// below `mm::MMAP_BASE` (`0x1_0000_0000`) — the AArch64 loader's own
/// `PIE_BASE`-collides-with-its-mmap-region bug
/// (`userspace/apk-tools/docs/PIE_LOADER.md` "Change 2") cannot recur here
/// for the boring reason that the two were never close: an anonymous mmap
/// from musl's TLS setup (`__copy_tls`) lands 3.75 GiB away from this base,
/// not on top of it.
const PIE_BASE: u64 = 0x1000_0000;

/// Auxiliary-vector keys this kernel supplies.
///
/// Spelled here rather than pulled from `akuma_elf::types::auxv`, which is
/// private to that crate. When the parse/place split in the module header
/// happens, these move with it.
const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_ENTRY: u64 = 9;

/// How many frames one process may own.
///
/// A fixed array rather than a `Vec`: this is teardown bookkeeping on a path
/// that runs under memory pressure, and the kernel's own rule is that the best
/// code allocates nothing.
///
/// For a loader-built process this only has to cover **image + stack** — static
/// musl busybox is ~275 pages of segments plus the 128-page stack — and its
/// `mmap`/heap frames are tracked separately (`mm::release_anon_frames`).
///
/// But a **`fork`** child's `FrameSet` holds a copy of *every* mapped user page
/// — image, stack, heap and all (`Process::fork_from`) — because there is no
/// CoW and teardown must find them. A long-lived interactive shell can map well
/// past 512 pages, and hitting the cap makes `fork` return `ENOMEM` (the user
/// sees `sh: can't fork: Out of memory` on a box with 500 MiB free). 2048
/// covers busybox plus a generous heap; the buffer is heap-allocated (see
/// [`FrameSet`]), so raising this only costs 8 bytes/entry per live process. A
/// CoW `fork` plus the region list this array stands in for removes the cap
/// entirely (§3.26.5).
pub const MAX_PROC_FRAMES: usize = 2048;

/// Every physical frame a process owns, so teardown can give them all back.
///
/// `AddressSpace::free` releases page *tables*, not the pages they point at —
/// deliberately, since the tables are its own and the leaves are the caller's.
/// This is that caller's half of the bargain.
///
/// The `MAX_PROC_FRAMES`-word buffer lives on the **heap** (a boxed slice), not
/// inline: at `[usize; 2048]` an inline array made `Process` 16 KiB, and every
/// by-value move of one — `Process::new` returning two into a tuple,
/// `fork_from` returning `Option<Process>` on a 32 KiB task stack — overflowed
/// the stack into a `#PF` that looked like anything but. One 16 KiB heap block
/// per live process is the fix; teardown reads it and never allocates.
pub struct FrameSet {
    frames: alloc::boxed::Box<[usize]>,
    len: usize,
}

impl Default for FrameSet {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameSet {
    #[must_use]
    pub fn new() -> Self {
        // A repeat `vec!` allocates and fills on the heap directly — no
        // MAX_PROC_FRAMES-word stack temporary on the way to the box.
        Self {
            frames: alloc::vec![0usize; MAX_PROC_FRAMES].into_boxed_slice(),
            len: 0,
        }
    }

    /// Record a frame. Returns false when full, which the caller must treat as
    /// a load failure — a frame that is not recorded is a frame that leaks.
    pub fn push(&mut self, pa: usize) -> bool {
        if self.len >= MAX_PROC_FRAMES {
            return false;
        }
        self.frames[self.len] = pa;
        self.len += 1;
        true
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Return every frame to the PMM and forget them.
    pub fn free_all(&mut self) {
        for &pa in self.frames.iter().take(self.len) {
            akuma_pmm::free_page(pa, 0);
        }
        self.len = 0;
    }
}

/// What a successful load produced.
pub struct LoadedImage {
    /// `base + e_entry` — where `enter_user_mode` jumps. Equal to `e_entry`
    /// for an `ET_EXEC` image (`base` is 0 there).
    pub entry: u64,
    /// Page-aligned end of the highest `PT_LOAD`. Where a `brk` heap would
    /// start; recorded now because it is free to compute here and impossible to
    /// recover later.
    pub end_va: u64,
    /// How many `PT_LOAD` segments were placed.
    pub segments: usize,
    /// Where the program header table ended up in the mapped image, for
    /// `AT_PHDR` — found by locating the `PT_LOAD` segment whose file range
    /// covers `e_phoff` and translating through *that* segment's own
    /// `p_vaddr`/`p_offset`, **not** `base + e_phoff` (only correct when the
    /// covering segment links `p_vaddr == p_offset`, which a traditional
    /// `ET_EXEC` linked at a high base does not — see `load`'s own comment
    /// at the computation for the crash that shortcut produced). No
    /// `PT_PHDR` segment search: nothing here needs one to exist, and
    /// `apk.static` (this feature's reason to exist) does not carry one.
    /// `0` if no `PT_LOAD` segment covers `e_phoff` — the hand-linked
    /// `userspace/amd64/hello`/`fdprobe` probes don't bother, and neither
    /// reads its own auxv, so this is a fallback rather than a load failure.
    pub phdr_addr: u64,
    /// `e_phnum`, for `AT_PHNUM`.
    pub phnum: u16,
    /// `e_phentsize`, for `AT_PHENT` — always 56 on ELF64 (checked at parse
    /// time), but passed through rather than hard-coded a second place.
    pub phent: u16,
}

/// Round `v` up to the next multiple of `to`.
const fn align_up(v: u64, to: u64) -> u64 {
    v.div_ceil(to) * to
}

/// Permissions for a `PT_LOAD`, from its `p_flags`.
///
/// There is no read bit: x86 page tables cannot express "not readable" for a
/// present page, so `PF_R` is implied and a segment without it would be
/// readable anyway. Saying so here is more honest than pretending to enforce it.
const fn segment_prot(p_flags: u32) -> Prot {
    Prot {
        write: p_flags & PF_W != 0,
        exec: p_flags & PF_X != 0,
        user: true,
    }
}

/// The permissions a page needs to satisfy both `a` and `b`.
///
/// Only reachable when two segments share a page, which a page-aligned link
/// never does. It is the union rather than "first writer wins" because
/// under-permitting is a fault at run time in code that looks correct, whereas
/// the over-permitting case that actually matters — W+X — is refused outright by
/// the caller.
const fn widen(a: Prot, b: Prot) -> Prot {
    Prot { write: a.write || b.write, exec: a.exec || b.exec, user: a.user || b.user }
}

/// Copy `src` into `space` at virtual address `va`, page by page.
///
/// Writes through the physmap rather than through `va` itself: the address
/// space being filled is not the active one — that is the whole point of
/// building it before `CR3` ever names it — so its virtual addresses mean
/// nothing to the CPU right now.
fn write_user(space: &AddressSpace, va: u64, src: &[u8]) -> bool {
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
                phys_ptr::<u8>(pa).add(off),
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
/// Zeroing on allocation is what implements `p_memsz > p_filesz`: the caller
/// copies only the file-backed bytes and the rest is already zero. It is also
/// what stops a recycled frame handing ring 3 whatever the previous owner left
/// in it — the same rule the demand-paging handler follows.
fn map_range(
    space: &AddressSpace,
    frames: &mut FrameSet,
    start: u64,
    end: u64,
    prot: Prot,
) -> Result<(), &'static str> {
    let mut va = start;
    while va < end {
        if let Some(existing) = space.prot(va as usize) {
            // A page a previous segment already placed. Only reachable from an
            // unaligned link; see `widen`.
            let want = widen(existing, prot);
            if want.write && want.exec {
                return Err("segments share a page and would make it writable+executable");
            }
            if want != existing {
                let pa = space.translate(va as usize).ok_or("mapped page has no frame")?;
                if !space.map(va as usize, pa, want, MemAttr::WriteBack) {
                    return Err("could not widen a shared page's permissions");
                }
            }
        } else {
            let pa = akuma_pmm::alloc_page().ok_or("out of frames loading a segment")? as u64;
            // SAFETY: a fresh PMM frame, reached through the physmap.
            unsafe { core::ptr::write_bytes(phys_ptr::<u8>(pa), 0, PAGE_SIZE) };
            // Recorded before it is mapped: a frame the set does not know about
            // is a frame that leaks, and `map` can fail.
            if !frames.push(pa as usize) {
                akuma_pmm::free_page(pa as usize, 0);
                return Err("image needs more frames than a process may own");
            }
            if !space.map(va as usize, pa, prot, MemAttr::WriteBack) {
                return Err("could not map a segment page");
            }
        }
        va += PAGE_SIZE as u64;
    }
    Ok(())
}

/// Parse `image` and place its `PT_LOAD` segments into `space`.
///
/// On failure the frames allocated so far are still recorded in `frames`, so
/// the caller can free them; nothing is freed here, because a half-loaded image
/// whose frames were silently reclaimed would leave `space`'s page tables
/// pointing at memory the PMM had handed to someone else.
pub fn load(
    image: &[u8],
    space: &AddressSpace,
    frames: &mut FrameSet,
) -> Result<LoadedImage, &'static str> {
    if image.len() < ELF64_EHDR_SIZE {
        return Err("image is shorter than an ELF64 header");
    }
    let ident = elf::file::parse_ident::<LittleEndian>(&image[..EI_NIDENT])
        .map_err(|_| "bad ELF identification")?;
    if ident.1 != Class::ELF64 {
        return Err("not ELF64");
    }
    let ehdr = FileHeader::parse_tail(ident, &image[EI_NIDENT..ELF64_EHDR_SIZE])
        .map_err(|_| "bad ELF header")?;

    if ehdr.e_machine != EM_X86_64 {
        return Err("not an x86-64 image");
    }
    let is_pie = match ehdr.e_type {
        ET_EXEC => false,
        ET_DYN => true,
        _ => return Err("not ET_EXEC or ET_DYN"),
    };
    // `p_vaddr` values in a PIE start near 0 (linked as if loaded at 0); an
    // `ET_EXEC` image links at its real address, so `base` is 0 there and
    // every `base + p_vaddr` below is unchanged from before this existed.
    let base = if is_pie { PIE_BASE } else { 0 };
    // PN_XNUM keeps the real count in shdr[0].sh_info. Nothing here comes near
    // 65535 segments, so reject it rather than mis-read the table as that many.
    if ehdr.e_phnum == elf::abi::PN_XNUM {
        return Err("PN_XNUM segment count unsupported");
    }

    let entsize = usize::from(ehdr.e_phentsize);
    if entsize != 56 {
        return Err("bad e_phentsize for ELF64");
    }
    let table_size = entsize
        .checked_mul(usize::from(ehdr.e_phnum))
        .ok_or("program header table overflow")?;
    let phoff = usize::try_from(ehdr.e_phoff).map_err(|_| "program header offset overflow")?;
    let phend = phoff.checked_add(table_size).ok_or("program header table overflow")?;
    let phdrs = image.get(phoff..phend).ok_or("program header table past end of image")?;
    let segments = SegmentTable::new(ehdr.endianness, ehdr.class, phdrs);

    // A dynamically-linked binary (one that names a real interpreter) is
    // still refused outright — see the module header's "What it refuses".
    // Checked before placing anything, so a rejected image leaves no frames
    // behind to unwind.
    if segments.iter().any(|ph| ph.p_type == PT_INTERP) {
        return Err("PT_INTERP present — this kernel has no dynamic linker, only static-PIE");
    }

    let mut placed = 0usize;
    let mut end_va = 0u64;

    for ph in segments.iter() {
        if ph.p_type != PT_LOAD {
            continue;
        }
        if ph.p_memsz == 0 {
            continue;
        }
        if ph.p_filesz > ph.p_memsz {
            return Err("segment p_filesz exceeds p_memsz");
        }

        let seg_end = base
            .checked_add(ph.p_vaddr)
            .and_then(|v| v.checked_add(ph.p_memsz))
            .ok_or("segment wraps the address space")?;
        if seg_end > USER_VA_LIMIT {
            return Err("segment is not in the lower half");
        }

        let prot = segment_prot(ph.p_flags);
        if prot.write && prot.exec {
            return Err("segment is both writable and executable");
        }

        let vaddr = base + ph.p_vaddr;
        let start = vaddr & !(PAGE_SIZE as u64 - 1);
        let end = align_up(seg_end, PAGE_SIZE as u64);
        map_range(space, frames, start, end, prot)?;

        if ph.p_filesz > 0 {
            let off = usize::try_from(ph.p_offset).map_err(|_| "segment file offset overflow")?;
            let len = usize::try_from(ph.p_filesz).map_err(|_| "segment file size overflow")?;
            let file_end = off.checked_add(len).ok_or("segment file range overflow")?;
            let bytes = image.get(off..file_end).ok_or("segment data past end of image")?;
            if !write_user(space, vaddr, bytes) {
                return Err("segment page vanished between mapping and copying");
            }
        }

        placed += 1;
        end_va = end_va.max(end);
    }

    if placed == 0 {
        return Err("image has no PT_LOAD segments");
    }
    let entry = base.checked_add(ehdr.e_entry).ok_or("entry point overflows the address space")?;
    if entry == 0 || entry >= USER_VA_LIMIT {
        return Err("entry point is not a user address");
    }
    if space.prot(entry as usize & !(PAGE_SIZE - 1)).is_none_or(|p| !p.exec) {
        return Err("entry point is not in an executable segment");
    }

    // `AT_PHDR`: the runtime address file offset `e_phoff` maps to — **not**
    // simply `base + e_phoff`. That shortcut is only correct when the
    // covering segment's `p_vaddr` equals its `p_offset`, true for a PIE
    // linked near 0 (`apk.static`'s segment 0 is `p_vaddr=0, p_offset=0`) but
    // false for a traditional `ET_EXEC` image linked at a high base — busybox
    // links at `0x40_0000` with `p_offset=0` for the same segment, so
    // `base + e_phoff` (`0x40`, `e_phoff`'s literal value — the ELF header is
    // exactly 64 bytes) is not even a mapped address, and busybox faulted on
    // it at `cr2=0x40` the first time this shipped with the shortcut. Found
    // instead by locating the `PT_LOAD` segment whose *file* range covers
    // `e_phoff` and translating through *that* segment's own
    // `p_vaddr`/`p_offset` pair, which is correct for both shapes.
    //
    // Falls back to 0 rather than failing the whole load when no segment
    // covers it — `userspace/amd64/hello`/`fdprobe`'s hand-rolled
    // `user.ld` does not bother mapping the file's first few bytes into any
    // `PT_LOAD` (nothing in either program reads its own program headers),
    // and refusing to load them over an auxv entry neither one looks at
    // would be exactly backwards. A real static-PIE binary's own toolchain
    // always covers this in practice — `apk.static` does — so this fallback
    // is never expected to be what a genuine `ET_DYN` load hits.
    let phdr_addr = segments
        .iter()
        .filter(|ph| ph.p_type == PT_LOAD)
        .find_map(|ph| {
            let seg_off_end = ph.p_offset.checked_add(ph.p_filesz)?;
            if ehdr.e_phoff < ph.p_offset || ehdr.e_phoff >= seg_off_end {
                return None;
            }
            base.checked_add(ph.p_vaddr)?.checked_add(ehdr.e_phoff - ph.p_offset)
        })
        .unwrap_or(0);

    Ok(LoadedImage { entry, end_va, segments: placed, phdr_addr, phnum: ehdr.e_phnum, phent: ehdr.e_phentsize })
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
/// The most argv entries the initial stack builder will place. A shell invoked
/// as `sh -c "<cmd>"` needs three; a generous bound catches a runaway without a
/// heap allocation on the spawn path.
pub const MAX_ARGV: usize = 16;

/// The most envp entries the initial stack builder will place. `execve` from a
/// shell hands the child its whole environment; 32 covers a login shell's
/// `PATH`/`HOME`/`TERM`/… with headroom, and bounds the copy without a heap
/// allocation (same reasoning as [`MAX_ARGV`]).
pub const MAX_ENVP: usize = 32;

/// Words in the fixed word block: argc, argv ptrs + NULL, envp ptrs + NULL,
/// and five auxv pairs (`AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, `AT_PAGESZ`,
/// `AT_ENTRY`, `AT_NULL` — six, not five; the name undercounts by one on
/// purpose-adjacent history, see the `aux` word count below which is the
/// number that actually matters).
const STACK_WORDS_MAX: usize = 1 + (MAX_ARGV + 1) + (MAX_ENVP + 1) + 12;

/// As [`load`]'s return value: `AT_PHDR`/`AT_PHNUM`/`AT_PHENT` are what let a
/// static-PIE binary (`apk`) find its own program headers and self-relocate
/// — see the module header's "Static-PIE" section. An `ET_EXEC` image (every
/// other program on this target) ignores them; they cost three more auxv
/// words and nothing else.
pub fn build_stack(
    space: &AddressSpace,
    frames: &mut FrameSet,
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
    map_range(space, frames, base, top, Prot::USER_RW)?;

    // The argv then envp strings sit at the very top, NUL-terminated, packed
    // downward. `arg_va[i]` / `env_va[i]` is where each string's bytes land.
    let mut arg_va = [0u64; MAX_ARGV];
    let mut env_va = [0u64; MAX_ENVP];
    let mut cursor = top;
    for (slot, a) in arg_va.iter_mut().zip(argv) {
        cursor -= a.len() as u64 + 1;
        *slot = cursor;
    }
    for (slot, e) in env_va.iter_mut().zip(envp) {
        cursor -= e.len() as u64 + 1;
        *slot = cursor;
    }
    // Round the whole string blob down to 16 so the word block below starts
    // aligned without a second adjustment.
    let strings_base = cursor & !0xf;

    // argc, one pointer per argv entry, argv NULL, one per envp entry, envp
    // NULL, six auxv pairs (AT_PHDR, AT_PHENT, AT_PHNUM, AT_PAGESZ, AT_ENTRY,
    // AT_NULL).
    let words = 1 + argv.len() + 1 + envp.len() + 1 + 12;
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
    for (i, &va) in arg_va.iter().take(argv.len()).enumerate() {
        put(1 + i, va);
    }
    put(1 + argv.len(), 0); // argv terminator
    for (i, &va) in env_va.iter().take(envp.len()).enumerate() {
        put(2 + argv.len() + i, va);
    }
    put(2 + argv.len() + envp.len(), 0); // envp terminator
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
    put(aux + 9, img.entry);
    put(aux + 10, AT_NULL);
    put(aux + 11, 0);

    for (&va, a) in arg_va.iter().zip(argv).chain(env_va.iter().zip(envp)) {
        if !write_user(space, va, a) || !write_user(space, va + a.len() as u64, &[0]) {
            return Err("could not write argv/envp");
        }
    }
    if !write_user(space, rsp, &buf[..words * 8]) {
        return Err("could not write the initial stack frame");
    }
    Ok(rsp)
}

//! `mmap`, `munmap` and `mprotect` for ring 3, over the `akuma-mmap` region table.
//!
//! The one memory syscall family a userspace allocator needs. `libakuma`'s
//! global allocator is mmap-based — it had a `brk` arm once and that was removed
//! — so without this no program that allocates can run, which is every program
//! more complex than the Stage L probe.
//!
//! # What changed on 2026-09-07 (item B1/B2)
//!
//! This module used to be an eager bump allocator with no region table, and
//! every one of its limits was a wall `rustc` hits. All five are gone:
//!
//! | before | now |
//! |---|---|
//! | `MAX_MAPPING` 64 MiB, anything larger `EINVAL` | bounded by the VA window, `ENOMEM` past it |
//! | `EAGER_MAX_PAGES = usize::MAX` — no lazy path at all | [`akuma_config::MMAP_EAGER_MAX_PAGES`], demand-paged past it |
//! | one **global** bump `NEXT_VA`, never reused | per-address-space first-fit over the region list |
//! | `MAP_FIXED` refused | honoured, and it replaces what it lands on |
//! | `munmap` could not clip or split | [`akuma_mmap::detach_eager_regions_in_range`] |
//!
//! and `mprotect`, which accepted-and-did-nothing, now splits regions and
//! re-permissions the pages that are actually present.
//!
//! # What a file mapping is here (2026-09-07)
//!
//! `mmap(MAP_PRIVATE, fd)` used to be `ENOSYS`, on the reasoning that serving it
//! as anonymous memory "would look like a working call and hand the caller a
//! file full of zeros". That reasoning was right about the **danger** and too
//! broad about the **remedy**: the zeros are what a mapping with no fill path
//! produces, not what a private file mapping is.
//!
//! `MAP_PRIVATE` asks for a private copy of the file's bytes whose writes
//! nobody else can see. This target gives exactly that: each page is allocated,
//! zeroed, and then overwritten from the file ([`populate_file_page`]).
//!
//! One refusal is left: a **writable `MAP_SHARED`** file mapping, whose writes
//! must reach the file and every other mapper. A private copy would accept the
//! write and drop it, which is the original objection in its true scope.
//!
//! One pinned divergence comes with it: a mapping never sees a write made to
//! the file after the `mmap` — `MAP_PRIVATE` leaves that unspecified on Linux
//! too.
//!
//! # What changed on 2026-09-13: lazy, and shared
//!
//! Until then a file mapping was always **eager** — every frame allocated and
//! every page read before `mmap` returned — and every mapping held its own
//! copy. Both are gone, and they were one change: a cache is only reachable
//! from a path that fills pages one at a time.
//!
//! | before | now |
//! |---|---|
//! | eager: 311 MB read and 76 000 frames allocated to start `rustc` | demand-paged from [`MmapRegion::file`] by [`fault_in`], 16 pages a fault |
//! | 207 ms inside one `sys_mmap`, holding the BKL (`[BKL] stuck … tag=9`) | 0 — nothing is read at `mmap` time |
//! | two processes mapping one file held two sets of frames | one frame per `(mount, inode, offset)`, `akuma-fpcache` |
//! | a mapping larger than free memory was `ENOMEM` at `mmap` | it faults, and evicts, like the AArch64 kernel |
//!
//! Measured: four concurrent mappers of a 311 MB library, 1.17 GiB resident
//! before and 303 MiB after. The cost is the opposite case — a pass that
//! touches *every* page of what it maps pays a fault per 16 pages for bytes one
//! sequential read would have delivered, and comes out ~16% behind
//! (`docs/archive/RUST_TOOLCHAIN_AMD64.md` § session 5). The win is not a
//! faster fill; it is not filling.
//!
//! # The decisions are shared crates, not local
//!
//! Which *kind* of mapping a request asks for — anonymous or file-backed, lazy
//! or eager, shared-writable, a `PROT_NONE` reservation — is
//! [`akuma_syscalls_mem::mmap::plan`]. The region algebra (clip, split, inherit)
//! is `akuma-mmap`. The protection vocabulary is `akuma_mmap::Prot`, which
//! [`akuma_mmu::PteProt::from_region`] encodes into x86 PTE bits. All three
//! are host-tested and shared with the AArch64 kernel, so this target cannot
//! drift from it on exactly the arguments where Linux compatibility is subtle.
//!
//! What stays here is the half that is genuinely per-architecture: allocating
//! frames, writing page tables, and the fault that populates a lazy page.
//!
//! # Where the page tables live, and where the region list lives
//!
//! The tables are `Process::space`, an `akuma_mmu::UserAddressSpace` behind
//! `ProcAddressSpace`'s lock, reached through
//! [`crate::usermode::with_current_address_space`]. Until step 5a every function
//! here took a bare `root: u64` from `paging::active_root()` — which answers with
//! **`CR3`** whoever asks, so a `munmap` on a kernel thread walked the kernel's
//! own tables. That could only be guarded by remembering to call
//! `have_address_space()` first; there is no root to pass now, and the accessor
//! answers `None` instead.
//!
//! The lock order in this module is **regions → address space → PMM**, in that
//! direction only. `fault_in` and `dontneed_range` are the two that hold both.
//!
//! The region list is `Process::regions`, behind its own lock, reached through
//! [`crate::usermode::with_current_regions`]. `akuma-mmap` cannot hold it — the
//! crate has an empty `[dependencies]` table and cannot lock, allocate a frame,
//! edit a page table or name a process, which is precisely what makes it
//! trustworthy. A region list belongs to an *address space*, and a
//! `clone(CLONE_VM)` thread shares one, which is why it is keyed by
//! `proc_slot` and not by thread.
//!
//! # Frames are the ledger's, not the region's
//!
//! `MmapRegion::frames` is left **empty** here and `pages` carries the extent —
//! the CoW-inherited shape the crate documents and explicitly supports. Frame
//! ownership on this target is `akuma_user_space::FrameLedger`, which counts VAs
//! per frame and is what teardown walks. Since step 5a that ledger lives
//! **inside** the address space rather than beside it as a second `Process`
//! field, so the walk that clears a PTE and the ledger entry that stops claiming
//! its frame are reached through one object and edited in one descent
//! ([`unmap_range`]). A second frame list inside the region would be a second
//! answer to the same question, and the two would drift the first time a CoW
//! break swapped a frame.

use akuma_mmu::{LeafAction, PteProt};

use crate::phys::phys_ptr;
use crate::usermode;
use akuma_mmap::{FileBacking, MmapRegion, PhysFrame, Prot};
#[cfg(not(feature = "no-tests"))]
use akuma_selftest::Suite;
use alloc::vec::Vec;

use crate::fd::errno;
use crate::serial;

const PAGE_SIZE: u64 = 4096;

/// `PROT_WRITE` / `PROT_EXEC`, from the shared flag tables rather than restated
/// here — the same constants the AArch64 kernel dispatches on.
use akuma_syscalls_linux::flags::prot::{PROT_EXEC, PROT_WRITE};

/// Where automatically-placed mappings start.
///
/// Well above where a static binary is linked (`0x40_0000`) and above the
/// dynamic linker's `INTERP_BASE` (`0x4000_0000`), so an image and its mappings
/// cannot meet.
/// `pub` within a private module — the crate is the only reader. 5b slice 1:
/// the akuma-exec `Process` registered per process carries the same mmap
/// window in its `ProcessMemory`, so the two placers cannot drift
/// (`usermode.rs::register_exec_process`).
pub const MMAP_BASE: usize = 0x1_0000_0000;

/// One past the last address the automatic placer will hand out.
///
/// 112 TiB, which leaves the initial stack (`ELF_STACK_TOP`, just under 128 TiB)
/// and 16 TiB of clearance below it outside the window entirely. The stack is
/// not in the region table — the loader places it — so the window has to avoid
/// it by construction rather than by collision test.
const MMAP_TOP: usize = 0x7000_0000_0000;

/// The largest `len` this call will consider, for
/// [`akuma_syscalls_mem::mmap::len_too_large`].
///
/// A bound rather than trust: `len` comes from a ring-3 register and drives
/// `0..pages` loops. Past this the answer is `ENOMEM` — Linux's answer for a
/// length it cannot map — rather than the old `EINVAL`, which was wrong for a
/// caller that simply asked for more memory than exists.
const MMAP_VA_SPAN: usize = MMAP_TOP - MMAP_BASE;

/// One past the last user address. Above this is the kernel's half.
///
/// Only `MAP_FIXED` needs it: automatic placement is confined to
/// `[MMAP_BASE, MMAP_TOP)` already, and a fixed request may legitimately land
/// outside that window (over its own image, say) but never in the upper half.
///
/// Note `akuma_syscalls_mem::mmap::fixed_overlaps_kernel_va` — the guard that
/// exists because the Go runtime commits arenas with `MAP_FIXED` — is **vacuous
/// on this target** and deliberately not called: this kernel has no identity map
/// in the lower half at all (`phys.rs`: physmap at PML4 256, devices at 257,
/// image at 511), so there is nothing of the kernel's down here to overlap. The
/// half-space check below is the whole guard.
const USER_VA_LIMIT: usize = 0x0000_8000_0000_0000;

/// How many pages this target maps eagerly before `plan` calls a mapping lazy.
///
/// `akuma_config::MMAP_EAGER_MAX_PAGES`, the same 16 the AArch64 kernel uses,
/// read from the shared config rather than restated — this is a tuning knob and
/// two copies of a knob is one knob and one bug.
///
/// It was `usize::MAX` until 2026-09-07, which said "never lazy" honestly
/// because there was no region table for the `#PF` handler to consult. There is
/// now, and [`fault_in`] is what consults it.
const EAGER_MAX_PAGES: usize = akuma_config::MMAP_EAGER_MAX_PAGES;

/// First-fit a free `pages`-page range in `[MMAP_BASE, MMAP_TOP)`.
///
/// # Reuse, and what it costs
///
/// The bump allocator this replaces never handed out an address twice, so a
/// use-after-`munmap` faulted instead of landing silently in a later mapping.
/// That is a genuinely useful property and reuse gives it up — but the bump was
/// **global across every process**, so a 64 MiB mapping in one program consumed
/// address space for all of them, and nothing ever gave any of it back. `rustc`
/// reserves gigabytes; a monotonic global cursor is not a thing it can run on.
///
/// The mitigation is that the window is 112 TiB and first-fit only reuses a hole
/// something was explicitly unmapped from, so an address is recycled long after
/// it went away rather than immediately.
///
/// # Why it sorts, and why that is still no allocation
///
/// The region list is not *kept* in address order (`detach_eager_regions_in_range`
/// pushes survivors onto the end), so this sorts it before scanning. The first
/// version refused to — "a gap scan would have to sort, which means a `Vec` per
/// `mmap`, on the path a program allocating memory takes" — and walked instead:
/// propose a candidate, and on an overlap jump the candidate past whatever it hit
/// and **restart the walk**. `cand` strictly increases on every restart, so that
/// terminated "in at most one pass per region", which is true, is what the doc
/// comment said, and reads like a linear bound. It is O(n²) per call.
///
/// That cost is invisible until a process accumulates four figures of regions and
/// is then the whole machine. The in-guest self-host build stopped making forward
/// progress compiling `zerocopy`: its `rustc` had ~1500 mappings, mostly
/// single-page (musl's mallocng `mmap`s one group at a time), every one of its
/// `State: R` threads was still running and every `mmap` was still completing —
/// each one paying for every mapping the process had ever made.
/// `docs/archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §5 is the hunt.
///
/// `sort_unstable_by_key` is pattern-defeating quicksort: in place, **no
/// allocation** — which is the whole objection above, and it does not apply — and
/// adaptive, so the sorted list this leaves behind costs a linear pass to
/// re-confirm on the next call rather than a full sort. Sorting the caller's list
/// is sound because nothing reads it in order: regions never overlap (`MAP_FIXED`
/// unmaps its range first, every other placement comes from here), so the
/// `find(|r| r.contains(va))` lookups elsewhere in this module have at most one
/// answer whatever order they walk in.
///
/// The scan is then a single pass. `cand` is the high-water mark of every region
/// seen so far, and because the list is sorted, the first region starting at or
/// past `cand + len` proves every byte below it is free.
fn find_free_va(regions: &mut [MmapRegion], pages: usize) -> Option<usize> {
    find_free_va_scan(regions, pages).0
}

/// [`find_free_va`], plus how many regions the scan looked at.
///
/// The count is the complexity assertion, and it exists because a boot suite can
/// demand bounded *work* where it cannot reliably time anything:
/// `va_placement_check` walks a thousand-region staircase — the shape that used
/// to cost one full pass per region — and checks the scan touched each region at
/// most once. Nothing in the kernel proper reads it.
fn find_free_va_scan(regions: &mut [MmapRegion], pages: usize) -> (Option<usize>, usize) {
    let Some(len) = pages.checked_mul(PAGE_SIZE as usize) else {
        return (None, 0);
    };
    regions.sort_unstable_by_key(|r| r.start_va);
    let mut cand = MMAP_BASE;
    let mut examined = 0usize;
    for r in regions.iter() {
        let Some(end) = cand.checked_add(len) else {
            return (None, examined);
        };
        if end > MMAP_TOP {
            return (None, examined);
        }
        examined += 1;
        // Sorted, so every region after this one starts at or past it: one that
        // begins at or past the candidate's end proves the gap below is clear.
        if r.start_va >= end {
            return (Some(cand), examined);
        }
        // Otherwise the candidate moves past this region's end. `max` is what
        // covers a region lying entirely *below* the candidate — a `MAP_FIXED`
        // mapping under `MMAP_BASE`, or one an earlier region already subsumed —
        // which must not drag the candidate backwards.
        cand = cand.max(r.start_va.saturating_add(r.len_bytes()));
    }
    let Some(end) = cand.checked_add(len) else {
        return (None, examined);
    };
    if end > MMAP_TOP {
        (None, examined)
    } else {
        (Some(cand), examined)
    }
}

/// Is the caller a slotted user task with an address space of its own?
///
/// The one question every effect in this module is gated on. It is asked
/// through the region accessor rather than `current_proc_slot` so there is a
/// single definition of "has an address space" — a slot that exists but holds
/// no `Process` is not one, and the two spellings disagreed about that.
fn have_address_space() -> bool {
    usermode::with_current_regions(|_| ()).is_some()
}

/// The protection the page table should be told for a region page at `pa`.
///
/// [`PteProt::from_region`] plus one rule that only the kernel can know: **a
/// frame that is still CoW-shared never becomes writable in the PTE**, however
/// writable the region says it is. It is mapped read-only and marked instead, so
/// the first write faults and `cow_write_fault` breaks the sharing.
///
/// Without this an `mprotect(PROT_WRITE)` over a forked page would hand both
/// processes a writable mapping of one frame and their memory would silently
/// diverge — the exact failure copy-on-write exists to prevent, reintroduced
/// through the one syscall that is allowed to *raise* a permission.
fn pte_prot_for(prot: Prot, pa: usize) -> (PteProt, bool) {
    let p = PteProt::from_region(prot);
    if p.write && akuma_pmm::cow_ref_get(pa) > 0 {
        (PteProt { write: false, ..p }, true)
    } else {
        (p, false)
    }
}

/// `mmap(addr, len, prot, flags, fd, offset)`.
pub fn sys_mmap(addr: u64, len: u64, prot: u64, flags: u64, fd: u64, offset: u64) -> u64 {
    if len == 0 {
        return errno::EINVAL;
    }
    let (prot32, flags32) = (prot as u32, flags as u32);
    // `fd` arrives as a `u64` and Linux's is a signed `int`: the "no file"
    // sentinel is -1, which is `u64::MAX` here, and truncating to `i32` is what
    // turns it back into the -1 `plan` compares against.
    let fd32 = fd as i32;

    // Alignment first, before anything else looks at the request: the AArch64
    // kernel does the same, and the ordering is asserted by its boot suite.
    if akuma_syscalls_mem::mmap::fixed_addr_unaligned_einval(addr as usize, flags32) {
        return errno::EINVAL;
    }
    // A fixed placement is checked against the user half here, beside the
    // alignment rule and ahead of everything with an effect — the same reason
    // the alignment check is first. `addr == 0` is a fixed request for the null
    // page, which is never a real one and would make a null dereference
    // succeed. Saturating, because `len` is a ring-3 register and a wrap would
    // turn "spans the whole address space" into "fits".
    if flags32 & akuma_syscalls_linux::flags::map::MAP_FIXED != 0 {
        let want = addr as usize;
        if want < PAGE_SIZE as usize
            || want.saturating_add(len as usize) > USER_VA_LIMIT
        {
            return errno::EINVAL;
        }
    }
    if akuma_syscalls_mem::mmap::len_too_large(len as usize, MMAP_VA_SPAN) {
        return errno::ENOMEM;
    }

    let pages = len.div_ceil(PAGE_SIZE) as usize;
    let plan = akuma_syscalls_mem::mmap::plan(prot32, flags32, fd32, pages, EAGER_MAX_PAGES);

    // File-backed mappings (2026-09-07). The refusal that used to live here was
    // right about the danger and too broad about the remedy — see the module
    // header's "What a file mapping is here".
    if plan.is_file_backed {
        // Still refused: a **writable `MAP_SHARED`** file mapping. Writes
        // through it must become visible in the file and to every other mapper,
        // which needs a write-back path and one shared frame per file page —
        // that is the page cache, and it is genuinely not here. Serving it as a
        // private copy would accept the write and silently drop it.
        if plan.is_shared_writable {
            return errno::ENOSYS;
        }
        // Linux requires a page-aligned offset and says `EINVAL` otherwise.
        // Ahead of the descriptor probe because it is decidable from the
        // arguments alone, which is the ordering rule the rest of this function
        // follows and what lets the boot suite assert it with no open file.
        if !offset.is_multiple_of(PAGE_SIZE) {
            return errno::EINVAL;
        }
        // A socket, pipe or directory has no bytes to map.
        if !crate::fd::is_regular_file(fd) {
            return errno::EACCES;
        }
    }

    // W^X, enforced here as it is in the ELF loader: `PteProt` offers no
    // writable-and-executable constructor, and a JIT is not something this
    // target supports. Kept ahead of everything that has an effect so a refused
    // request changes nothing.
    if prot32 & PROT_WRITE != 0 && prot32 & PROT_EXEC != 0 {
        return errno::EINVAL;
    }
    let region_prot = Prot::from_prot(prot32);

    // Everything past this point has an **effect on an address space**, so there
    // has to be one. Kept below the argument checks rather than above them
    // because the ordering is load-bearing: the AArch64 kernel refuses a
    // malformed request before it resolves a process, its boot suite asserts
    // that, and a kernel-test caller with no current task must see `EINVAL`
    // where Linux gives `EINVAL` — not `ESRCH`.
    if !have_address_space() {
        return errno::ESRCH;
    }

    let fixed = flags32 & akuma_syscalls_linux::flags::map::MAP_FIXED != 0;
    let byte_len = pages * PAGE_SIZE as usize;

    let base = if fixed {
        let want = addr as usize;
        // Rounding `len` up to a page can push the end past the half-space that
        // the raw `len` cleared above, so the rounded extent is re-checked here.
        if want.saturating_add(byte_len) > USER_VA_LIMIT {
            return errno::EINVAL;
        }
        // `MAP_FIXED` **replaces**: whatever is there goes away first, mappings
        // and region records alike. Doing this before reserving is what keeps
        // the new region from being clipped by the teardown of the old one.
        unmap_range(want, want + byte_len);
        want
    } else {
        // Without `MAP_FIXED` an address is a hint, and hints are advisory.
        let Some(found) = usermode::with_current_regions(|regions| find_free_va(regions, pages))
            .flatten()
        else {
            return errno::ENOMEM;
        };
        found
    };

    // Reserve, then populate. The region goes in **first**, under the lock, so a
    // concurrent `mmap` on another core cannot pick the same range; the frames
    // are allocated after the lock is dropped, so the PMM is never entered from
    // inside it. A `MAP_SHARED|MAP_ANONYMOUS` region is marked as such —
    // `fork` must share it by identity rather than copy-on-write, or a child's
    // write becomes invisible to the parent.
    // **A read-only file mapping is demand-paged** (2026-09-13), which is the
    // single largest thing this target was doing differently from the AArch64
    // kernel and the reason a `cargo` build here was memory- and I/O-bound.
    //
    // Eagerly, `mmap`ping `librustc_driver.so` allocated ~76 000 frames and read
    // 300 MB off ext2 before the call returned — for a library the linker then
    // touches a few megabytes of. Every page of that was paid for twice: once in
    // wall-clock inside the `mmap` (measured 2.2 µs/page, holding the BKL, which
    // is where the `[BKL] stuck … tag=9` storms came from) and once in physical
    // memory that could not be reclaimed while the mapping lived.
    //
    // What makes it possible is the record below: `MmapRegion::file` remembers
    // *which* file and *where in it*, so [`fault_in`] can answer a page later.
    // The identity is `(mount_id, inode)` and not the fd — `ld.so` closes the
    // descriptor the moment the mapping exists, and every fault after that has
    // no fd to ask.
    //
    // Gated on the same `akuma_config::MMAP_FILE_BACKED_LAZY` the AArch64 kernel
    // reads, and `plan.file_lazy_eligible`, which excludes writable `MAP_SHARED`
    // — refused outright above on this target. A file with no inode identity
    // (`file_identity` answers `None`) stays eager: there is nothing to fault
    // against.
    let lazy_file: Option<FileBacking> = if akuma_config::MMAP_FILE_BACKED_LAZY
        && plan.file_lazy_eligible
    {
        crate::fd::file_identity(fd).map(|(mount_id, inode, size)| FileBacking {
            mount_id,
            inode,
            offset: offset as usize,
            // What is left of the file from this mapping's own offset — the
            // rest of the mapping is zero-fill. A mapping that starts past EOF
            // is all zeros, which is what `mmap(2)` says and what
            // `saturating_sub` gives.
            filesz: size.saturating_sub(offset as usize),
        })
    } else {
        None
    };

    let mut region = MmapRegion::inherited_with_prot(base, pages, region_prot);
    if plan.shared_anon {
        region = region.shared_anon();
    }
    if let Some(file) = lazy_file {
        // The pin **before** the region is published: from the moment a fault
        // can reach this mapping, the inode it names has to be one the
        // filesystem will not free and reissue under it. That is root cause #2
        // of the AArch64 self-host `rustc` ICE, arriving here with the lazy path
        // that makes it reachable (`docs/archive/SELFHOST_ZERO_PAGE_HUNT.md`).
        pin_mapping_inode(file.inode);
        region = region.file_backed(file);
    }
    usermode::with_current_regions(|regions| regions.push(region));

    if lazy_file.is_some() {
        // Nothing is allocated and nothing is read. `fault_in` fills pages from
        // the file as they are touched, which for a shared object is a few per
        // cent of it.
        return base as u64;
    }

    // **Pinned divergence: a `MAP_SHARED | MAP_ANONYMOUS` mapping is never lazy
    // here**, whatever `plan` says. Such a region is shared with a `fork` child
    // *by identity* — same frames, mapped writable in both — and `fork_from`
    // does that by walking the parent's present leaves. A page that has not been
    // faulted in yet is not a leaf, so each side would demand-page its own
    // private frame and the mapping would silently behave like `MAP_PRIVATE`.
    // Sharing a not-yet-existing page needs a backing object this target does
    // not have; populating up front is the honest alternative, and a shared
    // anonymous mapping is a coordination area rather than a big reservation.
    if plan.use_lazy && !plan.shared_anon {
        // Nothing is allocated. The pages arrive through `fault_in` on first
        // touch, and a `PROT_NONE` reservation never gets any at all — which is
        // the whole point of reserving.
        return base as u64;
    }

    // A file-backed mapping is filled from the file; an anonymous one is zeroed.
    // `plan.use_lazy` is false for every file mapping (the crate decides that),
    // so control only reaches here for a file after the lazy return above.
    let file_source = plan.is_file_backed.then_some((fd, offset as usize));

    // Duration instrumentation, measured against the TSC — not
    // `net::uptime_us`, whose LAPIC tick does not advance inside a syscall
    // (`IF` clear), so a bare counter read here would measure 0
    // (`sched.rs`'s frozen-clock note). TEMPORARY: the `cargo -j4` guest fill
    // and console saw 429 `[BKL] stuck … tag=9` lines in one boot, all of
    // them this loop holding the BKL; this print says how long one mmap
    // actually holds it and at which file sizes the cost sits.
    let t0 = unsafe { core::arch::x86_64::_rdtsc() };
    // **File fills are batched.** One `file_bytes_at` call per 4 KiB page made
    // ext2 re-resolve the inode, re-derive block mappings and allocate its
    // `phys_blocks` scratch `Vec` per page — the measured 2.2 µs/page (8 MB in
    // 4.4 ms) was mostly that fixed cost, paid 76 000 times for one
    // `librustc_driver` mapping. Reading a 64 KiB chunk and fanning it out to
    // 16 frames amortizes it away; a transient bounded `Vec`, one per file
    // mapping, is the only allocation added.
    //
    // The chunk read answers the same bytes the per-page reads did, and that
    // equivalence rests on two things this loop must do and the per-page path
    // got for free:
    //
    // - **The buffer carries its own zeros.** `file_bytes_at` takes a `&mut
    //   [u8]` and fills a prefix; `populate_file_page` zeroed the frame first,
    //   so a short answer at EOF left zeros behind it. Here the buffer is
    //   reused across chunks, so anything past the `n` bytes read is the
    //   *previous* chunk's data unless it is cleared — the file would be
    //   mapped with a 60 KiB slice of itself repeated at the tail. Cleared
    //   below on every read, including the `n == 0` whole-chunk-past-EOF case.
    // - **A `Vec` with reserved capacity is still empty.** `try_reserve` sets
    //   capacity, not length, and `&mut v` derefs to a zero-length slice: the
    //   read would fill nothing, the fan-out would index past the end and the
    //   kernel would panic on the first file mapping of 16 pages or more.
    //   `resize` is what makes the buffer a buffer.
    //
    // TEMPORARY instrumentation note: `[mmap-t]` below measures the whole
    // loop, so the A/B is direct (2026-09-13: 2048 pages 4408 µs before).
    const CHUNK_PAGES: usize = 16;
    let chunk_bytes = CHUNK_PAGES * PAGE_SIZE as usize;
    // A failed allocation just means the per-page path below runs; the mmap
    // still succeeds, which is the right shape for a transient scratch buffer
    // on a path that must not turn memory pressure into a failed syscall.
    let mut chunk: Option<Vec<u8>> = file_source.and_then(|_| {
        let mut v: Vec<u8> = Vec::new();
        if v.try_reserve(chunk_bytes).is_ok() {
            v.resize(chunk_bytes, 0);
            Some(v)
        } else {
            None
        }
    });
    let mut batch_failed = false;
    let mut i = 0usize;
    while i < pages {
        let va = base + i * PAGE_SIZE as usize;
        match file_source {
            Some((fd, off)) if chunk.is_some() && i + CHUNK_PAGES <= pages => {
                let buf = chunk.as_mut().expect("guarded by the arm above");
                // `None` is "not a regular file" — a failure, exactly as it was
                // per page. A short answer is not: it is the tail of the file,
                // whose remainder `mmap(2)` specifies as zero.
                match crate::fd::file_bytes_at(fd, off + i * PAGE_SIZE as usize, buf) {
                    None => {
                        batch_failed = true;
                        break;
                    }
                    Some(n) => buf[n.min(chunk_bytes)..].fill(0),
                }
                for j in 0..CHUNK_PAGES {
                    let src = &buf[j * PAGE_SIZE as usize..(j + 1) * PAGE_SIZE as usize];
                    if populate_file_page_from(va + j * PAGE_SIZE as usize, region_prot, src).is_none() {
                        batch_failed = true;
                        break;
                    }
                }
                // Checked here and not only by the `for` above: a `break` out of
                // the fan-out leaves this loop's own condition untouched, and
                // without this the fill would carry on allocating frames for
                // every remaining page of a mapping already being torn down.
                if batch_failed {
                    break;
                }
                i += CHUNK_PAGES;
            }
            other => {
                let filled = match other {
                    Some((fd, off)) => {
                        populate_file_page(va, region_prot, fd, off + i * PAGE_SIZE as usize)
                    }
                    None => populate_page(va, region_prot),
                };
                if !filled {
                    batch_failed = true;
                    break;
                }
                i += 1;
            }
        }
    }
    if batch_failed {
        // Out of memory partway through. Unlike the pre-region version,
        // which leaked the pages it had already mapped because it had no
        // record of them, this can undo exactly what it did.
        unmap_range(base, base + byte_len);
        return errno::ENOMEM;
    }
    if pages >= 16 && MMAP_TRACE.load(core::sync::atomic::Ordering::Relaxed) {
        // TEMPORARY instrumentation (2026-09-13), now gated: this fired on
        // every mapping of 16 pages or more, and rustc mmaps constantly — an
        // in-guest build wrote hundreds of these lines to the UART *while
        // compiling*. The print, not the timing, was the cost; `rdtsc` is a
        // few cycles and stays. Flip `mm::MMAP_TRACE` for the A/B.
        let dt = unsafe { core::arch::x86_64::_rdtsc() }.wrapping_sub(t0);
        let hz = crate::lapic::tsc_hz();
        let us = if hz != 0 { dt / (hz / 1_000_000) } else { 0 };
        serial::puts("  [mmap-t] pages=");
        serial::put_dec(pages as u64);
        serial::puts(" file=");
        serial::put_dec(u64::from(file_source.is_some()));
        serial::puts(" us=");
        serial::put_dec(us);
        serial::puts("\n");
    }
    base as u64
}

/// Keep a demand-paged mapping's file alive for as long as the mapping is.
///
/// Delegates to the address space, which is where the pins live and why:
/// `akuma_mmu::UserAddressSpace::pin_mapped_inode`. It is the **leader's**
/// address space — the same owner `with_current_regions` answers for — so a
/// `CLONE_THREAD` thread's `mmap` pins where its region record went, not into a
/// view that dies with the thread.
fn pin_mapping_inode(inode: u32) {
    usermode::with_current_address_space(|uas| uas.pin_mapped_inode(inode));
}

/// Allocate, zero, map and record one anonymous page at `va`.
///
/// The single place a frame becomes part of a user address space on this path,
/// which is why the ledger update is here and not at the three call sites.
/// A frame the ledger does not know about is a frame `Process::free` will not
/// release — the leak that made every post-`fork` `mmap` permanent.
fn populate_page(va: usize, prot: Prot) -> bool {
    let Some(frame) = akuma_pmm::alloc_page() else {
        return false;
    };
    // Zero before mapping: a recycled frame otherwise hands ring 3 whatever the
    // previous owner left in it. The same rule the ELF loader follows.
    // SAFETY: a fresh PMM frame, reached through the physmap.
    unsafe { core::ptr::write_bytes(phys_ptr::<u8>(frame as u64), 0, PAGE_SIZE as usize) };
    // A brand-new frame is unshared, so `pte_prot_for`'s CoW rule cannot fire;
    // it is used anyway so there is one answer to "what bits does this region
    // get" rather than two that could drift.
    let (pte, cow) = pte_prot_for(prot, frame);
    // The allocation and the zeroing are outside the address-space hold; only
    // the PTE edit and the ledger entry are inside it. `map_and_track_pte`
    // records the frame before it maps and untracks it if the map fails, which
    // is the `track_anon_frame` this used to do afterwards — and getting the
    // order that way round is what stops a failed map leaving the ledger
    // claiming a page nothing points at.
    if usermode::with_current_address_space(|uas| {
        uas.map_and_track_pte(va, PhysFrame::new(frame), pte, cow)
    }) != Some(true)
    {
        akuma_pmm::free_page(frame, 0);
        return false;
    }
    true
}

/// Allocate, **fill from a file**, map and record one page at `va`.
///
/// [`populate_page`]'s sibling, and the difference is the whole reason
/// file-backed `mmap` was refused here until 2026-09-07: this function is what
/// stops the mapping being anonymous memory wearing a file's name.
///
/// # The failure this is built to make impossible
///
/// `mm.rs`'s header used to say a file mapping was `ENOSYS` because serving one
/// as anonymous memory "would look like a working call and hand the caller a
/// file full of zeros". That danger is real and it is *not* removed by having a
/// fill path — it is removed by making the fill path unable to silently skip.
/// So:
///
/// - The frame is zeroed **first**, then overwritten with what the file has.
///   Zero is therefore the value of a byte past EOF and of nothing else.
/// - [`crate::fd::file_bytes_at`] returns `None` for a descriptor that is not a
///   regular file, and that is a **failure of this function**, not a zero-byte
///   fill. `sys_mmap` also refuses such a descriptor up front; the second check
///   is here because this is the function that would otherwise produce the
///   zeros, and a guard at the point of harm outlives a guard at the caller.
/// - A short answer is *not* a failure: it is a page that straddles EOF, whose
///   tail `mmap(2)` specifies as zero.
///
/// # What this is not
///
/// A page cache. Every mapping gets its **own copy** of every page, so two
/// processes mapping one file hold two sets of frames and a write through
/// `MAP_PRIVATE` cannot be seen by anyone — which is what `MAP_PRIVATE` means,
/// so it is correct and merely expensive. `MAP_SHARED` writable is refused in
/// `sys_mmap` precisely because *that* one needs the sharing to be real.
fn populate_file_page(va: usize, prot: Prot, fd: u64, offset: usize) -> bool {
    let Some(frame) = akuma_pmm::alloc_page() else {
        return false;
    };
    // SAFETY: a fresh PMM frame, reached through the physmap, and no other
    // reference to it exists until it is mapped below.
    let page = unsafe {
        core::slice::from_raw_parts_mut(phys_ptr::<u8>(frame as u64), PAGE_SIZE as usize)
    };
    // Zeroed before the fill, never instead of it.
    page.fill(0);
    if crate::fd::file_bytes_at(fd, offset, page).is_none() {
        // Not a regular file. Refuse rather than map the zeros just written.
        akuma_pmm::free_page(frame, 0);
        return false;
    }
    let (pte, cow) = pte_prot_for(prot, frame);
    // The file read is deliberately **outside** the address-space hold: it takes
    // the descriptor table, and taking that lock underneath this one would be
    // the only place in this module where the two are ordered that way.
    if usermode::with_current_address_space(|uas| {
        uas.map_and_track_pte(va, PhysFrame::new(frame), pte, cow)
    }) != Some(true)
    {
        akuma_pmm::free_page(frame, 0);
        return false;
    }
    true
}

/// Allocate, **fill from an already-read buffer**, map and record one page.
///
/// [`populate_file_page`]'s batched twin — [`sys_mmap`]'s fill loop reads a
/// 64 KiB chunk once and hands each 4 KiB slice here, so ext2's per-call fixed
/// cost is paid per chunk instead of per page. Same rules: freshly allocated
/// frame (no stale contents), PTE edit and ledger entry under the
/// address-space hold, frame freed on a failed map.
fn populate_file_page_from(va: usize, prot: Prot, src: &[u8]) -> Option<PhysFrame> {
    let frame = akuma_pmm::alloc_page()?;
    // SAFETY: a fresh PMM frame, reached through the physmap, and no other
    // reference to it exists until it is mapped below.
    let page = unsafe {
        core::slice::from_raw_parts_mut(phys_ptr::<u8>(frame as u64), PAGE_SIZE as usize)
    };
    // Clamped rather than trusted: the caller slices a chunk buffer, and a page
    // is the most this frame can hold. A longer `src` is a caller bug, and the
    // kind that would otherwise write past the frame into the physmap.
    let n = src.len().min(PAGE_SIZE as usize);
    page[..n].copy_from_slice(&src[..n]);
    page[n..].fill(0);
    let (pte, cow) = pte_prot_for(prot, frame);
    if usermode::with_current_address_space(|uas| {
        uas.map_and_track_pte(va, PhysFrame::new(frame), pte, cow)
    }) != Some(true)
    {
        akuma_pmm::free_page(frame, 0);
        return None;
    }
    Some(PhysFrame::new(frame))
}

/// Service a not-present fault at `addr` from the region table — demand paging.
///
/// `true` when the faulting instruction can be re-executed. `false` means the
/// address is not inside any mapping of this process (or is inside a `PROT_NONE`
/// reservation), and the fault falls through to the handler's remaining arms.
///
/// # Why the region lock is held across the allocation here
///
/// [`sys_mmap`] deliberately populates *outside* the lock, because it may map
/// many pages. This maps exactly one, and holding the lock across it closes the
/// window where a concurrent `munmap` retires the region between the lookup and
/// the map — which would leave a page mapped into a range the process has been
/// told is gone. Lock order is regions → PMM, and nothing takes them the other
/// way round.
///
/// # `PROT_NONE` is a refusal, and it is a *grant* record being read
///
/// A reservation's pages must never be populated: that is what makes a guard
/// page guard. The read is safe against
/// `docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md` because every region this
/// target creates records its protection explicitly ([`MmapRegion`] is built
/// through `*_with_prot` on both the `mmap` and the fork-inherit path), so
/// `recorded_prot` is always `Some`. The `None` arm is unreachable and grants
/// rather than refuses anyway, which is the direction that trap says to fail in.
pub fn fault_in(addr: u64) -> bool {
    let page = (addr as usize) & !0xfff;

    // **Decide under the region lock; fill outside it.** The anonymous case
    // could do both inside, and did — a zero-fill takes no other lock. A file
    // fill reads ext2, and this module's lock order is regions -> address space
    // -> PMM with nothing else in it; adding a filesystem underneath the region
    // lock would make this the one place where a VFS lock is taken under it, and
    // the `read(2)` path that prefaults a lazy user buffer (`prefault_user_range`
    // below) arrives with filesystem state of its own. The window the release
    // opens is closed by the BKL: `idt.rs` takes it for the whole servicing
    // window, and a peer's `munmap` is a syscall, which cannot run without it.
    let Some((prot, file)) = usermode::with_current_regions(|regions| {
        let region = regions.iter().find(|r| r.contains(page))?;
        let prot = region.recorded_prot().unwrap_or(Prot::RW_NO_EXEC);
        if prot.is_none() {
            return None; // a reservation, or a guard page: a real fault
        }
        // Extent as well as identity: the readahead below must not run off the
        // end of the region it started in.
        Some((prot, region.file.map(|f| (f, region.start_va, region.pages))))
    })
    .flatten() else {
        return false;
    };

    match file {
        None => populate_page(page, prot),
        Some((file, region_start, region_pages)) => {
            fill_file_pages(page, prot, file, region_start, region_pages)
        }
    }
}

/// How many faults were served from a file, and how many pages that filled.
///
/// Counters rather than a log line: a demand-paged file mapping is *quiet* when
/// it works, and the failure mode that matters is not an error but the arm never
/// being taken at all — a regression to the eager path is invisible in every
/// other measurement, because eager mappings work too. They are reported by
/// [`demand_paging_report`] after the boot suite has run real programs, the same
/// argument [`crate::idt::USER_DEMAND_FAULTS`] exists for.
pub static FILE_DEMAND_FAULTS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
/// Pages filled from a file by [`fill_file_pages`], readahead included.
pub static FILE_PAGES_FILLED: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
/// Of those, how many cost **no frame and no read** because the shared
/// file-page cache already held the page. The ratio to
/// [`FILE_PAGES_FILLED`] is the deduplication actually achieved, which is the
/// number worth watching when several compilers run at once.
pub static FILE_PAGES_SHARED: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Enables the `[mmap-t]` per-mapping timing print (see `fill_file_pages`).
/// Off by default: the print fired on every ≥16-page mmap and an in-guest
/// rustc build flooded the UART with it while compiling. Same shape as
/// `usermode::SYSCALL_TRACE` — a diagnostics toggle, not a test-only gate.
pub static MMAP_TRACE: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// How many pages one file fault brings in.
///
/// A fault costs a `#PF`, a region-list walk and an ext2 call whose cost is
/// mostly fixed (inode resolve, block-map derivation, a scratch `Vec`), so
/// serving one page per fault pays that fixed cost per 4 KiB. Sixteen pages is
/// one 64 KiB read, which is also the chunk [`sys_mmap`]'s eager fill uses, and
/// it is deliberately **not** the AArch64 kernel's 256: this is the target where
/// the page is copied out of a heap buffer rather than shared out of a page
/// cache, so every page read ahead and not used is a frame allocated and a
/// memcpy performed for nothing. Raise it once pages can be shared.
const READAHEAD_PAGES: usize = 16;

/// Fill the faulting page — and the readahead window after it — from the file.
///
/// Returns `true` when **the faulting page** is present on return; a readahead
/// page that could not be filled is not a failure, it is just a page that will
/// fault later.
///
/// Pages already present are skipped rather than refilled: a sibling thread may
/// have faulted the same window, and re-populating a live page would leak the
/// frame under it and discard whatever ring 3 has written there.
fn fill_file_pages(
    page: usize,
    prot: Prot,
    file: FileBacking,
    region_start: usize,
    region_pages: usize,
) -> bool {
    use core::sync::atomic::Ordering;
    FILE_DEMAND_FAULTS.fetch_add(1, Ordering::Relaxed);
    let first = (page - region_start) / PAGE_SIZE as usize;
    let last = (first + READAHEAD_PAGES).min(region_pages);

    // One 64 KiB read for the whole window, fanned out to the frames — the same
    // amortization the eager fill does, for the same reason. A failed allocation
    // is not a failed fault: the per-page path below reads straight into each
    // frame and needs no buffer at all.
    let mut buf: Option<Vec<u8>> = {
        let want = (last - first) * PAGE_SIZE as usize;
        let mut v: Vec<u8> = Vec::new();
        if v.try_reserve(want).is_ok() {
            v.resize(want, 0);
            Some(v)
        } else {
            None
        }
    };
    if let Some(b) = buf.as_mut() {
        let (offset, _) = file.page_source(first);
        match crate::fd::file_bytes_by_inode(file.mount_id, file.inode, offset, b) {
            // The file is gone, or was never readable by inode. Fall back to the
            // per-page path, which fails the same way and one page at a time.
            None => buf = None,
            // Short at EOF, or nothing at all: the rest of the window is
            // zero-fill, and the buffer must say so rather than keep whatever
            // `resize` left there.
            Some(n) => b[n..].fill(0),
        }
    }

    // **Frame sharing**, the second half of the AArch64 win. Two `rustc`s
    // mapping one `librustc_driver.so` held two physical copies of every page
    // and read each of them off ext2 twice; deduplicating on
    // `(mount_id, inode, file offset)` collapses that to one fill and one frame
    // however many mappers there are, which is what stops `-j4` being *slower*
    // than `-j1` (`akuma-fpcache`'s crate docs).
    //
    // The eligibility rules are the crate's, restated in this target's
    // vocabulary rather than passed as AArch64 PTE bits:
    //
    // * **Read-only to ring 3.** A writable private file mapping would have to
    //   break copy-on-write before sharing; `ld.so`'s relocated data segments
    //   stay private. `PteProt::from_region` is the authority on what this
    //   region's pages will actually be mapped as.
    // * **Fully covered by file data** (below, per page). The page straddling
    //   EOF has a zero-fill tail whose length belongs to the *mapping*, so two
    //   mappers may legitimately disagree about its contents.
    // * **A resolved identity**, which a lazy region always has — it could not
    //   have been created without one.
    //
    // A cap of zero means the cache is off (`akuma_fpcache::init` never armed
    // it, or `SHARED_FILE_PAGES_ENABLED` is false), and then nothing here takes
    // a reference: a private page left with a global refcount of 1 would read
    // as CoW-shared to `pte_prot_for` and cost a copy on the first write.
    let region_pte = PteProt::from_region(prot);
    let sharing = !region_pte.write && akuma_fpcache::cap() > 0;

    let mut faulting_page_ok = false;
    for idx in first..last {
        let va = region_start + idx * PAGE_SIZE as usize;
        if akuma_mmu::is_current_user_range_mapped(va, 1) {
            if va == page {
                // A peer filled the very page this fault is about. Present is
                // present: return to ring 3 and let the access retry.
                faulting_page_ok = true;
            }
            continue;
        }
        let (offset, from_file) = file.page_source(idx);
        let share_this = sharing && from_file == PAGE_SIZE as usize;

        // A hit costs no frame, no read and no copy — just a reference and a
        // PTE. This is the whole point of the cache, and on the self-host build
        // it is the common case from the second `rustc` onwards.
        if share_this
            && let Some((frame, _needs_icache)) = akuma_fpcache::lookup_and_ref(
                file.mount_id,
                file.inode,
                offset,
                region_pte.exec,
            )
        {
            // `_needs_icache`: x86 has coherent instruction caches, so there is
            // no `ic ivau` counterpart to perform. The AArch64 caller acts on
            // this flag; ignoring it here is a property of the architecture, not
            // an omission.
            let ok = map_shared_file_page(va, prot, frame);
            if ok {
                FILE_PAGES_FILLED.fetch_add(1, Ordering::Relaxed);
                FILE_PAGES_SHARED.fetch_add(1, Ordering::Relaxed);
            }
            if va == page {
                faulting_page_ok = ok;
            }
            if !ok {
                break;
            }
            continue;
        }

        let filled = match buf.as_ref() {
            Some(b) => {
                let at = (idx - first) * PAGE_SIZE as usize;
                populate_file_page_from(va, prot, &b[at..at + from_file])
            }
            None => populate_file_page_by_inode(va, prot, file, offset, from_file),
        };
        if let Some(frame) = filled {
            FILE_PAGES_FILLED.fetch_add(1, Ordering::Relaxed);
            if share_this {
                // Two references cover this frame, and both are already in
                // place without another `cow_ref_inc` here: the mapping's own
                // — `populate_file_page_by_inode` installed it through
                // `map_and_track_pte`, which takes the reference its teardown
                // will drop — and the cache's own, taken inside `insert`.
                // An explicit third increment stood here until 2026-09-17 and
                // leaked one reference per freshly filled shared page: munmap
                // and the next write's `invalidate_inode` each dropped one of
                // three, stranding the frame at count 1 forever. Ten minutes
                // of `cargo build -j4` rewrote enough files to strand ~2.5 GiB
                // and push the guest to the OOM floor (the SMP=4 ring-3 `#UD`
                // in AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md). The AArch64
                // original never had it — its fill carries the allocation
                // reference into `adopt_user_frame(frame, owns_ref=true)`.
                //
                // `insert` may decline (over cap, or a peer published the same
                // page first). That is not an error and needs no undo: the
                // frame stays private with exactly the one reference this
                // mapping holds, which teardown balances.
                akuma_fpcache::insert(file.mount_id, file.inode, offset, frame, true);
            }
        }
        let ok = filled.is_some();
        if va == page {
            faulting_page_ok = ok;
        }
        if !ok {
            // Out of memory, or the file stopped answering. Stop reading ahead;
            // whether the fault itself succeeded is already recorded.
            break;
        }
    }
    faulting_page_ok
}

/// Map an already-filled **shared** file page into this address space.
///
/// The frame comes from [`akuma_fpcache::lookup_and_ref`], which has already
/// taken a global reference on this mapper's behalf — so this either installs
/// the page (the reference becomes the address space's) or gives the reference
/// back. There is no path where it is silently kept: a leaked reference pins a
/// frame until reboot, and a dropped one frees a page other processes are
/// executing from.
fn map_shared_file_page(va: usize, prot: Prot, frame: PhysFrame) -> bool {
    let (pte, cow) = pte_prot_for(prot, frame.addr);
    let outcome = usermode::with_current_address_space(|uas| {
        // `true` — the caller's reference. `adopt_user_frame` reports it back as
        // *surplus* when this address space already held the frame at another
        // VA, because teardown frees each distinct frame exactly once and a
        // second reference for a second VA would never be balanced.
        let surplus = uas.adopt_user_frame(frame, true);
        if uas.map_page_pte(va, frame.addr, pte, cow) {
            (true, surplus)
        } else {
            // Nothing was mapped, so this address space is not a mapper: undo
            // the adoption and hand the reference back.
            let _ = uas.remove_user_frame(frame);
            (false, true)
        }
    });
    match outcome {
        // No address space at all — a kernel thread has no business faulting
        // into one, and the reference must not be kept for it.
        None => {
            akuma_pmm::free_page(frame.addr, 0);
            false
        }
        Some((mapped, release)) => {
            if release {
                akuma_pmm::free_page(frame.addr, 0);
            }
            mapped
        }
    }
}

/// Allocate, fill **directly from the file**, map and record one page.
///
/// [`populate_file_page`] without a descriptor — the fault path's fallback for
/// when the readahead buffer could not be allocated, which is exactly the moment
/// a 64 KiB allocation is the wrong thing to insist on. Reads into the frame
/// through the physmap, so it needs no buffer of its own.
fn populate_file_page_by_inode(
    va: usize,
    prot: Prot,
    file: FileBacking,
    offset: usize,
    from_file: usize,
) -> Option<PhysFrame> {
    let frame = akuma_pmm::alloc_page()?;
    // SAFETY: a fresh PMM frame, reached through the physmap, and no other
    // reference to it exists until it is mapped below.
    let page = unsafe {
        core::slice::from_raw_parts_mut(phys_ptr::<u8>(frame as u64), PAGE_SIZE as usize)
    };
    // Zeroed before the fill, never instead of it: zero is the value of a byte
    // past EOF and of nothing else.
    page.fill(0);
    let want = from_file.min(PAGE_SIZE as usize);
    if want > 0
        && crate::fd::file_bytes_by_inode(file.mount_id, file.inode, offset, &mut page[..want])
            .is_none()
    {
        akuma_pmm::free_page(frame, 0);
        return None;
    }
    let (pte, cow) = pte_prot_for(prot, frame);
    if usermode::with_current_address_space(|uas| {
        uas.map_and_track_pte(va, PhysFrame::new(frame), pte, cow)
    }) != Some(true)
    {
        akuma_pmm::free_page(frame, 0);
        return None;
    }
    Some(PhysFrame::new(frame))
}

/// Demand-page every lazy page covering `[start, start + len)` so a **kernel**
/// access to that user range cannot fault. `false` if any page could not be
/// made present.
///
/// Registered into `akuma_user_access::set_prefault_hook` from
/// `boot::install_shared_sinks`. Every `akuma-syscalls-glue` arm that touches a
/// user buffer opens with `validate_user_ptr`, which is
/// `validate_user_range(.., Prefault::Yes)`: it walks the page table, and only
/// if the range is **not already present** does it call this. So the common
/// path pays nothing and this runs once per genuinely-lazy buffer.
///
/// # Why this target had no hook, and what that cost (4b batch 3a)
///
/// `amd64/src/uaccess.rs`'s header said, correctly when it was written: *"There
/// is no 'is it mapped' walk here and no prefault: this target has no lazy user
/// regions yet, so the copy either succeeds or faults, and the fault is
/// recovered. When lazy regions arrive, the walk goes here."* Lazy regions
/// arrived with B1 on 2026-09-07 and nothing came back to this sentence,
/// because nothing had to: this kernel's own `copy_to_user` still just copies,
/// and `idt.rs` services the `#PF` inline.
///
/// The shared arms do not work that way — they **ask first** — and the
/// unregistered hook is fail-closed, so the answer for a lazy page was
/// `EFAULT`. Folding `read(2)` made that reachable from ring 3 in the most
/// ordinary way there is: `apk` `mmap`s a buffer and `read`s a file into it,
/// i.e. into pages that have never been touched. It reported
/// `Unable to read database: v2 database format error` — a *file format*
/// complaint about a file the kernel had refused to read, which is the
/// wrong-layer symptom this whole fold keeps producing. The boot suite could
/// not see it (it runs under `BypassValidationGuard`, which returns before the
/// walk) and neither could `amd64_ring3_check.py`, whose workload reads into
/// libc heap buffers that are already resident.
///
/// # The two rules the loop encodes
///
/// - **Skip a page that is already present**, exactly as the AArch64
///   implementation does: re-populating one would leak the frame under it, and
///   a `PROT_NONE` guard must stay a guard. [`fault_in`] refuses a reservation
///   for the same reason, so the skip is belt and braces.
/// - **`false` on the first page that will not come in**, rather than a partial
///   success: the caller re-asserts `is_current_user_range_mapped` afterwards
///   anyway, and a half-filled range must reach it as a failure and not as a
///   copy into a hole.
pub fn prefault_user_range(start: usize, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    // The last *byte*, so a range ending exactly on a page boundary does not
    // pull in the page after it — and `checked_add` because `start + len` is
    // ring 3's arithmetic, not this kernel's.
    let Some(last_byte) = start.checked_add(len - 1) else {
        return false;
    };
    let page_mask = !(PAGE_SIZE as usize - 1);
    let last = last_byte & page_mask;
    let mut page = start & page_mask;
    loop {
        if !akuma_mmu::is_current_user_range_mapped(page, 1) && !fault_in(page as u64) {
            return false;
        }
        if page == last {
            return true;
        }
        page += PAGE_SIZE as usize;
    }
}

/// `munmap(addr, len)`.
pub fn sys_munmap(addr: u64, len: u64) -> u64 {
    if !addr.is_multiple_of(PAGE_SIZE) {
        return errno::EINVAL;
    }
    // **Divergence 1** of `akuma-syscalls-mem`, preserved rather than fixed:
    // `munmap(addr, 0)` unmaps one page here where Linux returns `EINVAL`.
    let byte_len = akuma_syscalls_mem::mmap::munmap_len(len as usize);
    let start = addr as usize;
    let Some(end) = start.checked_add(byte_len).filter(|e| *e <= USER_VA_LIMIT) else {
        return errno::EINVAL;
    };
    // Without a process there is no region list and no user address space. This
    // check is now belt-and-braces — [`unmap_range`] resolves the address space
    // through `with_current_address_space`, which answers `None` rather than
    // handing back the kernel's own `CR3` the way `paging::active_root()` did —
    // but it is kept because the **errno** is the point: a caller with no
    // process must see `ESRCH`, not the `0` a silently-skipped unmap would give.
    if !have_address_space() {
        return errno::ESRCH;
    }
    unmap_range(start, end);
    0
}

/// Retire `[start, end)`: detach the region records it covers, then unmap and
/// release every page that is actually present.
///
/// The two halves are separate on purpose. The **records** are clipped by
/// [`akuma_mmap::detach_eager_regions_in_range`], which splits a region the
/// range only partly covers and leaves the head and tail behind — the algebra
/// that had gone wrong before and now has host tests. The **pages** are found by
/// walking the page table, not the returned pieces, because a range may contain
/// mapped pages no region ever claimed (a `MAP_FIXED` landing on part of the ELF
/// image, say) and those must still be unmapped.
///
/// Frames go back through the ledger, never straight to the PMM:
/// `untrack_anon_frame` reports whether this address space held the last VA onto
/// the frame, and only then does `cow_ref_dec` get asked whether this was the
/// last address space. A frame the ledger does not track is left alone —
/// `Process::free` will release it — because freeing it here would be a double
/// free against that.
///
/// There is a **third** thing to release, and it was missing until 2026-09-17:
/// the `InodePin` a file-backed mapping took to keep its file's blocks alive
/// across an `unlink`. It lives in the address space, whose `Drop` releases it
/// on `exec` and exit — which covers the mapping's death only when the whole
/// address space dies with it. A `munmap` left the claim behind, and once
/// enough distinct inodes had been mapped to saturate the 1024-slot pin table,
/// `is_pinned` answered `true` for everything and ext2 stopped freeing blocks
/// on `unlink` at all. See `retain_mapped_inode_pins`.
fn unmap_range(start: usize, end: usize) {
    if end <= start {
        return;
    }
    // Records first, under the region lock, which is released before the page
    // walk below takes the address-space lock and the PMM. Lock order is
    // regions -> address space everywhere in this module.
    let _ = usermode::with_current_regions(|regions| {
        // Does the doomed range name a file at all? Almost no `munmap` does, and
        // the reconciliation below costs a scan of the region list per live pin,
        // so it is worth one overlap pass to skip it. A region that has no
        // `file` never contributed a pin.
        let unmaps_a_file = regions.iter().any(|r| {
            r.file.is_some() && r.start_va < end && r.start_va.saturating_add(r.len_bytes()) > start
        });
        let _pieces: Vec<_> = akuma_mmap::detach_eager_regions_in_range(regions, start, end);
        if unmaps_a_file {
            // The pins are per *inode*, so which ones to drop can only be
            // decided against the survivors: a program that maps one shared
            // object once per segment unmaps three regions and must keep the pin
            // until the third goes. Inside the region hold, which is the
            // established order (`with_current_address_space`: regions ->
            // address space, never the other way).
            let _ = usermode::with_current_address_space(|uas| {
                uas.retain_mapped_inode_pins(|inode| {
                    regions.iter().any(|r| r.file.is_some_and(|f| f.inode == inode))
                });
            });
        }
    });

    // One descent, clearing each leaf and dropping this address space's claim on
    // its frame in the same step. The ledger is reached through the walk's own
    // parameter rather than `usermode::untrack_anon_frame`, which would take
    // this very lock again and deadlock — and rather than a `Vec` of leaves,
    // which would allocate proportionally to residency on the one syscall that
    // runs when memory is short.
    let _ = usermode::with_current_address_space(|uas| {
        uas.rewrite_leaves_in_range(start, end, |ledger, leaf| {
            // Ledger first, then the global share count, then the free. A frame
            // this address space does not track is left alone — teardown will
            // release it — because freeing it here would be a double free
            // against that.
            if ledger.remove_user_frame(PhysFrame::new(leaf.pa))
                && akuma_pmm::cow_ref_dec(leaf.pa)
            {
                akuma_pmm::free_page(leaf.pa, 0);
            }
            LeafAction::Unmap
        });
    });
}

/// `mprotect(addr, len, prot)`.
///
/// # This used to be `return 0`
///
/// Accept-and-do-nothing, because there was no region table to re-permission
/// against. It meant a caller asking for *more* access than it had still saw the
/// original mapping (harmless, since the original was never less permissive) and
/// a caller asking for *less* was simply not honoured — which is every guard
/// page every allocator installs, silently absent.
///
/// # It splits; it does not annotate
///
/// [`akuma_mmap::mprotect_eager_regions_in_range`] splits a region the range
/// only partly covers rather than recording the new protection against the whole
/// of it. Recording it against the whole was the bug that killed `rustc`
/// mid-build on AArch64: a guard page `mprotect(PROT_NONE)`-ed inside a larger
/// mapping recorded `NONE` for every page of it
/// (`docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md`).
pub fn sys_mprotect(addr: u64, len: u64, prot: u64) -> u64 {
    if !addr.is_multiple_of(PAGE_SIZE) {
        return errno::EINVAL;
    }
    let prot32 = prot as u32;
    // The same W^X stance `sys_mmap` takes. Refusing here as well is what stops
    // `mprotect` being the back door onto a writable code page.
    if prot32 & PROT_WRITE != 0 && prot32 & PROT_EXEC != 0 {
        return errno::EINVAL;
    }
    if len == 0 {
        return 0;
    }
    let start = addr as usize;
    let byte_len = (len as usize).div_ceil(PAGE_SIZE as usize) * PAGE_SIZE as usize;
    let Some(end) = start.checked_add(byte_len).filter(|e| *e <= USER_VA_LIMIT) else {
        return errno::EINVAL;
    };

    let new_prot = Prot::from_prot(prot32);
    if usermode::with_current_regions(|regions| {
        akuma_mmap::mprotect_eager_regions_in_range(regions, start, end, new_prot);
    })
    .is_none()
    {
        return errno::ESRCH;
    }

    // Then the pages that are already present. A lazy page has no PTE to change
    // and does not need one — `fault_in` reads the region, which now says the
    // new thing.
    let _ = usermode::with_current_address_space(|uas| {
        uas.rewrite_leaves_in_range(start, end, |_ledger, leaf| {
            // Kernel pages are not ring 3's to re-permission. Nothing should map
            // one in a user range, and quietly rewriting it if something did is
            // the dangerous half of the two mistakes.
            if !leaf.prot.user {
                return LeafAction::Keep;
            }
            let (want, cow) = pte_prot_for(new_prot, leaf.pa);
            LeafAction::Reprotect(want, cow)
        });
    });
    0
}

/// `mremap(old_addr, old_size, new_size, flags)` — x86_64 syscall 25.
///
/// # It moves pages; it does not copy them
///
/// The AArch64 implementation allocates `new_pages` fresh frames and copies the
/// old bytes through a kernel bounce buffer. That is the only thing it can do
/// with the structures it has, and it has cost this tree two bugs: a copy loop
/// that `break`ed on the first lazy destination page and **silently truncated**
/// the mapping (`docs/archive/USER_COPY_FOLD.md` §5, which is why `mremapmove`
/// exists), and an `unwrap_or(NONE)` that turned "the source recorded no
/// protection" into "the source said `PROT_NONE`" and killed `rustc` mid-build.
///
/// This target has a region table and demand paging, so it can do the honest
/// thing instead: re-point each present page at the new virtual address and
/// leave the frame exactly where it is. No allocation, no copy, no bounce
/// buffer, and **no truncation is possible** — there is no loop that can stop
/// early and still look finished.
///
/// Sparsity falls out of that rather than needing a case. A source page that was
/// never faulted in is not present, so nothing is moved for it and the
/// destination page stays absent — where the next touch demand-pages a zero
/// frame, which is precisely what touching the source would have done. A copy
/// implementation has to *decide* what to do there; this one cannot get it
/// wrong.
///
/// The frame ledger is deliberately untouched: it counts virtual addresses per
/// frame within this address space, one VA goes away and one arrives, so the
/// count is unchanged. Removing and re-adding would be two edits with the same
/// net effect and one more place to get the order wrong.
///
/// # What is shared with AArch64
///
/// The decision: [`akuma_syscalls_mem::mremap::plan`] and
/// [`akuma_syscalls_mem::mremap::no_move_errno`], both host-tested. That
/// includes **divergence 5** — a shrink returns the old address with the tail
/// still mapped, where Linux unmaps it — and the `ENOMEM`-vs-`EFAULT` split for
/// a growth that may not move, which `mremap` implementations classically get
/// backwards.
pub fn sys_mremap(old_addr: u64, old_size: u64, new_size: u64, flags: u64) -> u64 {
    use akuma_syscalls_mem::mremap::{Plan, no_move_errno, plan};

    let (old_addr, old_size, new_size) = (old_addr as usize, old_size as usize, new_size as usize);

    // `MREMAP_FIXED` is refused rather than ignored.
    //
    // It asks for the mapping to land at a **specific** address and to replace
    // whatever is there. This target's placer chooses an address, so honouring
    // the flag is real work — and quietly returning a different address is a
    // confident wrong answer to a request that was explicit, which is the same
    // failure shape as answering `ENOSYS` where Linux answers `EINVAL`.
    //
    // **The AArch64 kernel currently ignores this flag** (`sys_mremap` in
    // `akuma-syscalls-glue` decodes only `MREMAP_MAYMOVE`), so this is a
    // deliberate divergence between the two targets and not an oversight in
    // either. Recorded here rather than fixed there: changing AArch64's answer
    // needs its own A/B, and nothing in the tree passes the flag today.
    if flags as u32 & akuma_syscalls_linux::flags::mremap::MREMAP_FIXED != 0 {
        return errno::EINVAL;
    }

    // Pure, and first — a caller with no address space must see the argument
    // errno rather than `ESRCH`, the same ordering `sys_mmap` and `sys_madvise`
    // follow and the reason the crate takes the VA limit as a parameter.
    let (new_pages, may_move) = match plan(old_addr, old_size, new_size, flags as u32, USER_VA_LIMIT)
    {
        Plan::Fail(e) => return e,
        Plan::InPlace => return old_addr as u64,
        Plan::Grow { new_pages, may_move } => (new_pages, may_move),
    };
    if !may_move {
        // Gated, as the crate documents: this probe is a page-table walk plus a
        // region scan, and running it on every growing `mremap` would cost that
        // on the common path.
        //
        // **Ahead of the `have_address_space` check on purpose.** With no
        // address space nothing is mapped, and "there is no mapping there"
        // (`EFAULT`) is both Linux's answer and more informative than `ESRCH`.
        // The AArch64 kernel arrives at the same answer by the same route — its
        // process lookup may yield `None` and `is_mapped` is then false.
        let is_mapped = usermode::with_current_address_space(|uas| uas.is_mapped(old_addr))
            .unwrap_or(false)
            || usermode::with_current_regions(|regions| {
                regions.iter().any(|r| r.contains(old_addr))
            })
            .unwrap_or(false);
        return no_move_errno(is_mapped);
    }

    if !have_address_space() {
        return errno::ESRCH;
    }

    let old_pages = old_size.div_ceil(PAGE_SIZE as usize);

    // Reserve under the lock, move outside it — the same trade `sys_mmap`
    // documents, and for the same reason: the page-table work must not run with
    // the region lock held any longer than the reservation needs.
    let Some(Some(base)) = usermode::with_current_regions(|regions| {
        let base = find_free_va(regions, new_pages)?;
        // A remap moves and resizes a mapping; it does not **re**protect it, so
        // the new region carries the old one's recorded protection — including
        // whether the old one recorded anything at all. Turning "the source said
        // nothing" into an explicit `PROT_NONE` is what killed `rustc` on the
        // AArch64 side (`docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md`), and
        // `MmapRegion::inherited` is the constructor that states nothing.
        let old_prot = regions
            .iter()
            .find(|r| r.start_va == old_addr)
            .and_then(MmapRegion::recorded_prot);
        regions.push(match old_prot {
            Some(prot) => MmapRegion::inherited_with_prot(base, new_pages, prot),
            None => MmapRegion::inherited(base, new_pages),
        });
        Some(base)
    }) else {
        return errno::ENOMEM;
    };

    // Re-point each present page. The range walk skips an absent subtree whole,
    // so a sparsely-touched 4 MiB source costs a walk of what is there rather
    // than 1024 four-level lookups — and it reports the PTE bits, which are
    // carried across unchanged so a CoW-marked page stays CoW-marked.
    //
    // Clearing the source is the walk's own `Unmap` and the destination is
    // mapped in a second pass over what it collected. That ordering is
    // load-bearing: the frame is the same one, so mapping the destination first
    // would leave it briefly at two VAs — which the ledger, whose count this
    // call deliberately does not touch, would then be one short of. It is also
    // why the collection cannot be avoided here the way `unmap_range` avoids
    // it: the walk holds `&mut` on the address space, so the new mapping cannot
    // be installed from inside it. The `Vec` is sized by the source's
    // *residency*, and `mremap` is not the syscall that runs out of memory.
    let mut moves: Vec<(usize, usize, PteProt, bool)> = Vec::new();
    let old_end = old_addr + old_pages * PAGE_SIZE as usize;
    let _ = usermode::with_current_address_space(|uas| {
        uas.rewrite_leaves_in_range(old_addr, old_end, |_ledger, leaf| {
            if !leaf.prot.user {
                return LeafAction::Keep;
            }
            moves.push((leaf.va, leaf.pa, leaf.prot, leaf.cow));
            LeafAction::Unmap
        });
        for &(va, pa, prot, cow) in &moves {
            let offset = va - old_addr;
            if !uas.map_page_pte(base + offset, pa, prot, cow) {
                // Out of page-table frames partway through. The pages already
                // moved are reachable at the new address and the region record
                // covers them, so nothing leaks and nothing is lost — the
                // caller simply gets a mapping with a hole, which the next
                // touch demand-pages.
                break;
            }
        }
    });

    // Retire the source. Its present pages are gone from the page table already,
    // so the walk inside `unmap_range` finds only what was never moved, and the
    // region record is clipped by `detach_eager_regions_in_range`.
    unmap_range(old_addr, old_end);
    base as u64
}

/// `madvise(addr, len, advice)` — x86_64 syscall 28.
///
/// # Why the errno matters more than the feature
///
/// This was not dispatched at all until 2026-09-07, so every advice answered
/// `ENOSYS` — and **that is not a neutral way to say "not implemented"**.
/// Linux's own answer for advice it does not support is `EINVAL`, and callers
/// read the difference: `redis-server` probes `MADV_FREE`, treats `EINVAL` as
/// "older kernel, presumably unaffected" and starts, but treats anything else as
/// a kernel it cannot trust and **exits**
/// (`docs/archive/LONG_ROAD_TO_REDIS.md` §5). So the smallest correct change
/// here was never an implementation; it was returning the errno Linux returns.
///
/// The decode is [`akuma_syscalls_mem::madvise::action`] — the same host-tested
/// function the AArch64 kernel dispatches on, so `MADV_FREE`'s deliberate
/// `EINVAL` and "every unrecognised advice reports success" cannot drift between
/// the two targets. Both are pinned by that crate's own tests.
///
/// # `MADV_WILLNEED` is a no-op here, deliberately
///
/// Only *anonymous* mappings are lazy on this target — `plan` never marks a file
/// mapping lazy, and [`sys_mmap`] populates every file page at `mmap` time — so
/// there is exactly one kind of page pre-faulting could touch, and pre-faulting
/// it installs a zero frame that reads exactly as the demand fault would have
/// produced. The content is identical either way; all pre-faulting changes is
/// *when* the memory is committed, and `madvise(2)` is explicitly advisory about
/// that. Doing nothing is conformant, and it keeps a ring-3 register from
/// driving an allocation loop over a range the caller only reserved.
///
/// **This stops being true the day a file mapping becomes lazy here.** At that
/// point `MADV_WILLNEED` must pre-fault anonymous lazy pages *only*: installing
/// a zeroed frame over a file-backed lazy page marks it present, so the fill
/// never runs and the file reads as zeros — the bug that silently zeroed every
/// weight page of a `llama.cpp` model mmap on AArch64
/// (`docs/archive/BKL_VFS_CARVE_OUT.md` §10). The eagerness is what makes the
/// no-op safe, so the two must move together.
pub fn sys_madvise(addr: u64, len: u64, advice: u64) -> u64 {
    use akuma_syscalls_mem::madvise::{self, Action};

    let (addr, len) = (addr as usize, len as usize);
    // Range validity FIRST — ahead of the advice decode and ahead of anything
    // that resolves a process, the same ordering rule `sys_mmap` follows. `len`
    // arrives straight from a ring-3 register: without this,
    // `madvise(addr, -1, MADV_DONTNEED)` is a page count of ~4.5e15
    // (`docs/archive/AKUMA_EXTRACT_MMAP.md` §10.1 defect A).
    if !madvise::range_fits_user_va(addr, len, USER_VA_LIMIT) {
        return errno::EINVAL;
    }

    match madvise::action(advice as i32) {
        // `MADV_FREE`. The whole point of this function — see the header.
        Action::Fail(e) => e,
        Action::Ignore | Action::Willneed => 0,
        Action::Dontneed => {
            if len == 0 {
                return 0;
            }
            if !have_address_space() {
                return errno::ESRCH;
            }
            let (start, pages) = madvise::dontneed_zero_range(addr, len);
            if pages == 0 {
                return 0;
            }
            dontneed_range(start, start + pages * PAGE_SIZE as usize);
            0
        }
    }
}

/// `MADV_DONTNEED` over `[start, end)`: make the caller's next read return zero
/// without making anyone else's do the same.
///
/// # The walk is the range walker, not a per-page loop
///
/// `UserAddressSpace::rewrite_leaves_in_range` visits only pages that are actually
/// **present**, and every absent page is `PageAction::Nothing` anyway — so the
/// walker's skip-an-absent-subtree-whole behaviour is not an optimisation here,
/// it is what bounds the work by what is mapped instead of by a length ring 3
/// chose. `MADV_DONTNEED` over a gigabyte-sized lazy reservation — which is the
/// commonest shape an allocator produces — costs a handful of reads.
///
/// # Only pages inside a recorded region are touched
///
/// A range handed to `madvise` may cover pages no `mmap` region ever claimed:
/// the ELF image's own text and data are mapped by the loader and are not in the
/// region list. Zeroing one of those would destroy the running program's code
/// **permanently**, because nothing on this target can re-read it — there is no
/// file backing and no refault path. Linux would drop the page and restore it
/// from the file, so skipping it here is *closer* to Linux than acting, not a
/// shortcut. It is a narrowing of what the AArch64 kernel does and is stated
/// rather than assumed.
///
/// A `PROT_NONE` reservation is skipped for the same reason `fault_in` refuses
/// to populate one: a guard page that quietly acquires a frame stops guarding.
///
/// # Zero in place, or break the sharing
///
/// [`akuma_syscalls_mem::madvise::dontneed_page_action`] decides, and the input
/// that matters is the **CoW share count**: a frame another address space can
/// see must not be zeroed in place, or a `fork` peer's live page is wiped. That
/// is the null-`Rc` corruption in `docs/archive/CARGO_HEAP_NULL_RC.md`, and this
/// target reaches it easily — `fork` is how every shell command starts here.
fn dontneed_range(start: usize, end: usize) {
    use akuma_syscalls_mem::madvise::{PageAction, dontneed_page_action};

    // One hold for the whole walk. Lock order is regions -> address space ->
    // PMM, the same direction `fault_in` takes, and nothing takes them the
    // other way round.
    let _ = usermode::with_current_regions(|regions| {
        let _ = usermode::with_current_address_space(|uas| {
        uas.rewrite_leaves_in_range(start, end, |ledger, leaf| {
            let (va, pa) = (leaf.va, leaf.pa);
            // A kernel page in a user range is not ring 3's to zero.
            if !leaf.prot.user {
                return LeafAction::Keep;
            }
            let Some(region) = regions.iter().find(|r| r.contains(va)) else {
                return LeafAction::Keep;
            };
            let prot = region.recorded_prot().unwrap_or(Prot::RW_NO_EXEC);
            if prot.is_none() {
                return LeafAction::Keep;
            }
            match dontneed_page_action(true, akuma_pmm::cow_ref_get(pa)) {
                // `for_each_leaf_in_range` only reports present pages, so the
                // unmapped arm is unreachable from here. Kept as an arm rather
                // than an `unwrap`: the decision belongs to the crate, and an
                // arm that stops being unreachable should compile, not panic.
                PageAction::Nothing => LeafAction::Keep,
                // This address space is the frame's only holder. Zeroing it is
                // indistinguishable from Linux's drop-and-refault and costs no
                // allocation.
                //
                // SAFETY: `pa` came from a present user leaf of this address
                // space, so it is a live 4 KiB frame reachable through the
                // physmap. Writing through the physmap rather than the user VA
                // is deliberate — the PTE may be read-only (a `PROT_READ`
                // region, or a CoW-demoted page whose peer has gone).
                PageAction::ZeroInPlace => {
                    unsafe {
                        core::ptr::write_bytes(phys_ptr::<u8>(pa as u64), 0, PAGE_SIZE as usize);
                    }
                    LeafAction::Keep
                }
                // Someone else maps this frame. Give this address space a
                // private zero frame and drop its share.
                PageAction::BreakSharing => {
                    let Some(fresh) = akuma_pmm::alloc_page() else {
                        // Advisory: out of memory means the page keeps its old
                        // contents, which is a worse answer than Linux's and a
                        // far better one than wiping a peer's live page.
                        return LeafAction::Keep;
                    };
                    // SAFETY: a fresh PMM frame, reached through the physmap.
                    unsafe {
                        core::ptr::write_bytes(phys_ptr::<u8>(fresh as u64), 0, PAGE_SIZE as usize);
                    };
                    // The old frame loses this VA. Ledger first, then the
                    // global share count, then the free — the same order
                    // `unmap_range` uses, and for the same reason: a frame this
                    // address space no longer tracks is not this address
                    // space's to hand back. Through the walk's own `ledger`
                    // rather than `usermode::{un,}track_anon_frame`, which would
                    // take the address-space lock this closure already runs
                    // under.
                    if ledger.remove_user_frame(PhysFrame::new(pa)) && akuma_pmm::cow_ref_dec(pa) {
                        akuma_pmm::free_page(pa, 0);
                    }
                    ledger.track_user_frame(PhysFrame::new(fresh));
                    let (pte, cow) = pte_prot_for(prot, fresh);
                    // `Remap` rather than a `map_page_pte` after the walk: the
                    // leaf slot is already in hand and the page tables above it
                    // already exist, so pointing it at the new frame is one
                    // store. It cannot fail, which is why the ledger edits above
                    // it have no undo arm.
                    LeafAction::Remap(fresh, pte, cow)
                }
            }
        });
        });
    });
}

#[cfg(not(feature = "no-tests"))]
/// The refusals and the arithmetic, which are the parts a guest program cannot
/// easily reach.
///
/// # Why this is not the whole test
///
/// It used to check *only* refusals, on the grounds that the success path is
/// exercised for real by every allocating program. As those refusals were
/// removed — `MAX_MAPPING`, `MAP_FIXED`, the lazy path — that reasoning would
/// have quietly left the module with no self-test at all, which is the "a boot
/// self-test that goes quiet is not a pass" trap. So the arms that disappeared
/// are replaced rather than deleted: the VA placer is now tested directly, and
/// the two refusals that remain are still asserted.
pub fn smoke_test(t: &mut Suite) {
    use akuma_syscalls_linux::flags::map::{MAP_ANONYMOUS, MAP_FIXED};
    const ANON: u64 = MAP_ANONYMOUS as u64;
    // -1, the "no file" sentinel, as it arrives from a 64-bit register.
    const NO_FD: u64 = u64::MAX;

    t.check_eq("mmap: zero length is EINVAL", sys_mmap(0, 0, 3, ANON, NO_FD, 0), errno::EINVAL);
    t.check_eq(
        "mmap: a length past the VA window is ENOMEM",
        sys_mmap(0, (MMAP_VA_SPAN + 1) as u64, 3, ANON, NO_FD, 0),
        errno::ENOMEM,
    );
    // File-backed mappings (2026-09-07). This arm used to assert that **every**
    // file-backed request was `ENOSYS`; that refusal is gone, so it is replaced
    // rather than deleted — a self-test that quietly stops covering the branch
    // it was written for is the "goes quiet is not a pass" trap.
    //
    // The three that remain are all decidable from the arguments, which is why
    // they can be asserted with no process and no open file.
    const MAP_SHARED: u64 = akuma_syscalls_linux::flags::map::MAP_SHARED as u64;
    t.check_eq(
        "mmap: a writable MAP_SHARED file mapping is still ENOSYS",
        sys_mmap(0, 4096, u64::from(PROT_WRITE) | 1, MAP_SHARED, 5, 0),
        errno::ENOSYS,
    );
    t.check_eq(
        "mmap: a file mapping at an unaligned offset is EINVAL",
        sys_mmap(0, 4096, 1, 0, 5, 1),
        errno::EINVAL,
    );
    // The descriptor probe. Nothing is open here, so fd 5 is not a regular file
    // and the request is refused **before** a frame is allocated — which is what
    // stops the zero-filled-file failure the old blanket refusal guarded
    // against. `populate_file_page` refuses the same thing a second time, at the
    // point where the zeros would otherwise be written.
    t.check_eq(
        "mmap: a file mapping on a descriptor that is not a file is EACCES",
        sys_mmap(0, 4096, 1, 0, 5, 0),
        errno::EACCES,
    );
    t.check_eq(
        "mmap: writable+executable is refused",
        sys_mmap(0, 4096, u64::from(PROT_WRITE | PROT_EXEC), ANON, NO_FD, 0),
        errno::EINVAL,
    );
    t.check_eq(
        "mmap: an unaligned MAP_FIXED address is EINVAL",
        sys_mmap(0x5000_1001, 4096, 3, ANON | u64::from(MAP_FIXED), NO_FD, 0),
        errno::EINVAL,
    );
    t.check_eq(
        "mmap: MAP_FIXED over the kernel half is EINVAL",
        sys_mmap(USER_VA_LIMIT as u64, 4096, 3, ANON | u64::from(MAP_FIXED), NO_FD, 0),
        errno::EINVAL,
    );
    t.check_eq(
        "munmap: an unaligned address is EINVAL",
        sys_munmap(0x1001, 4096),
        errno::EINVAL,
    );
    t.check_eq(
        "mprotect: writable+executable is refused",
        sys_mprotect(0x1_0000_0000, 4096, u64::from(PROT_WRITE | PROT_EXEC)),
        errno::EINVAL,
    );

    // Every one of these runs during the boot self-tests, where there is no
    // current process — so each must have been refused *before* reaching the
    // region list. A slotted caller is what `ESRCH` distinguishes, and a
    // refusal that returned it here would mean the ordering had slipped.
    t.check_eq(
        "mmap: a valid anonymous request with no process is ESRCH, not a mapping",
        sys_mmap(0, 4096, 3, ANON, NO_FD, 0),
        errno::ESRCH,
    );

    madvise_check(t);
    mremap_check(t);
    va_placement_check(t);
}

#[cfg(not(feature = "no-tests"))]
/// `mremap`: the argument answers, all of which must be decided before a
/// process is resolved.
///
/// The move itself needs an address space and is exercised for real by
/// `mremapmove` (`scripts/mem_suite.py`), which is where a truncation or a lost
/// page would show. What cannot be reached from a guest is the errno table, and
/// `mremap`'s is the one implementations classically get backwards.
fn mremap_check(t: &mut Suite) {
    use akuma_syscalls_linux::flags::mremap::{MREMAP_FIXED, MREMAP_MAYMOVE};
    const MAYMOVE: u64 = MREMAP_MAYMOVE as u64;
    const PAGE: u64 = 4096;

    t.check_eq(
        "mremap: a zero new size is EINVAL",
        sys_mremap(0x1_0000_0000, PAGE, 0, MAYMOVE),
        errno::EINVAL,
    );
    t.check_eq(
        "mremap: an unaligned old address is EINVAL",
        sys_mremap(0x1_0000_0001, PAGE, 2 * PAGE, MAYMOVE),
        errno::EINVAL,
    );
    t.check_eq(
        "mremap: an old address in the kernel half is EFAULT",
        sys_mremap(USER_VA_LIMIT as u64, PAGE, 2 * PAGE, MAYMOVE),
        errno::EFAULT,
    );
    // **Divergence 5**, pinned by `akuma-syscalls-mem`: a shrink returns the old
    // address and leaves the tail mapped, where Linux unmaps it. Asserted here
    // so a future "fix" is a deliberate change rather than a silent one.
    t.check_eq(
        "mremap: a shrink returns the old address unchanged",
        sys_mremap(0x1_0000_0000, 16 * PAGE, PAGE, MAYMOVE),
        0x1_0000_0000,
    );
    t.check_eq(
        "mremap: growth inside the last page is in place",
        sys_mremap(0x1_0000_0000, 1, PAGE, 0),
        0x1_0000_0000,
    );
    // Refused, not silently placed elsewhere — see `sys_mremap`'s comment.
    t.check_eq(
        "mremap: MREMAP_FIXED is refused rather than ignored",
        sys_mremap(0x1_0000_0000, PAGE, 2 * PAGE, MAYMOVE | u64::from(MREMAP_FIXED)),
        errno::EINVAL,
    );
    // The errno split. There is no process here, so nothing is mapped and the
    // no-move probe must answer `EFAULT` — "there is no mapping there" — rather
    // than `ENOMEM`, "no room to grow it".
    t.check_eq(
        "mremap: growing an unmapped address without MAYMOVE is EFAULT",
        sys_mremap(0x1_0000_0000, PAGE, 2 * PAGE, 0),
        errno::EFAULT,
    );
    // And a growth that may move needs an address space, which the boot suite
    // does not have. `ESRCH` is what says the argument checks were all passed.
    t.check_eq(
        "mremap: a growing move with no process is ESRCH",
        sys_mremap(0x1_0000_0000, PAGE, 2 * PAGE, MAYMOVE),
        errno::ESRCH,
    );
}

#[cfg(not(feature = "no-tests"))]
/// `madvise`: the errno choices, which are the whole of what this call is for on
/// a target where nothing but an anonymous mapping is ever lazy.
///
/// Every one of these is an *answer*, not an effect, and that is why they can be
/// asserted with no process: each must be decided before anything resolves an
/// address space. A `MADV_DONTNEED` is the one that has an effect, so it is the
/// one that reaches `ESRCH` here — and it reaching anything else would mean the
/// ordering had slipped, exactly as the `mmap` arm below asserts.
fn madvise_check(t: &mut Suite) {
    use akuma_syscalls_linux::flags::madvise::{
        MADV_DONTNEED, MADV_FREE, MADV_NORMAL, MADV_WILLNEED,
    };
    const PAGE: u64 = 4096;

    // The load-bearing one. `ENOSYS` here made `redis-server` exit rather than
    // start: it reads `EINVAL` as "older kernel, skip the check" and anything
    // else as a kernel it cannot trust (docs/archive/LONG_ROAD_TO_REDIS.md §5).
    t.check_eq(
        "madvise: MADV_FREE is EINVAL, not ENOSYS",
        sys_madvise(0x1_0000_0000, PAGE, MADV_FREE as u64),
        errno::EINVAL,
    );
    // The other half of the same rule: an advice nobody implements reports
    // success, because it is a hint. Both are pinned in `akuma-syscalls-mem`.
    t.check_eq(
        "madvise: an unrecognised advice reports success",
        sys_madvise(0x1_0000_0000, PAGE, 0xdead),
        0,
    );
    t.check_eq(
        "madvise: MADV_NORMAL reports success",
        sys_madvise(0x1_0000_0000, PAGE, MADV_NORMAL as u64),
        0,
    );
    // Advisory, and a no-op on this target — see `sys_madvise`'s header for the
    // condition under which that stops being true.
    t.check_eq(
        "madvise: MADV_WILLNEED reports success",
        sys_madvise(0x1_0000_0000, PAGE, MADV_WILLNEED as u64),
        0,
    );
    // The range guard, ahead of the advice decode. Without it this length is a
    // page count of ~4.5e15 and an unbounded loop reachable from ring 3
    // (docs/archive/AKUMA_EXTRACT_MMAP.md §10.1 defect A). Asserted with
    // `MADV_FREE` too, so a future implementation of it cannot skip the guard by
    // answering before the range is checked.
    t.check_eq(
        "madvise: a length past the user VA window is EINVAL",
        sys_madvise(0x1_0000_0000, u64::MAX, MADV_DONTNEED as u64),
        errno::EINVAL,
    );
    t.check_eq(
        "madvise: the range guard runs before the advice decode",
        sys_madvise(0x1_0000_0000, u64::MAX, MADV_FREE as u64),
        errno::EINVAL,
    );
    // And the effectful advice, which needs an address space it does not have
    // here. `ESRCH` rather than a silent 0 is what says the walk was reached.
    t.check_eq(
        "madvise: MADV_DONTNEED with no process is ESRCH",
        sys_madvise(0x1_0000_0000, PAGE, MADV_DONTNEED as u64),
        errno::ESRCH,
    );
    t.check_eq(
        "madvise: MADV_DONTNEED of zero length is a no-op",
        sys_madvise(0x1_0000_0000, 0, MADV_DONTNEED as u64),
        0,
    );
}

#[cfg(not(feature = "no-tests"))]
/// After the boot suite has run real programs: assert the lazy path was taken.
///
/// # Why this check exists
///
/// `mm::smoke_test` used to check only *refusals*, on the reasoning that the
/// success path is exercised for real by every allocating program. Removing the
/// refusals — `MAX_MAPPING`, `MAP_FIXED`, "never lazy" — would have left that
/// reasoning holding up nothing, which is the "a boot self-test that goes quiet
/// is not a pass" trap in exact form: demand paging could regress to *never
/// firing* and every other check in the suite would still be green, because an
/// eagerly-populated mapping works too. It would simply be the old kernel again,
/// silently.
///
/// So this runs **after** `busybox_test` / `execve_test` / `fork_test` /
/// `redirect_test` — shells, pipelines and a `fork`+`execve` — and asserts that
/// at least one not-present fault was serviced out of a region. musl's allocator
/// asks for arenas well past [`EAGER_MAX_PAGES`], so zero here means the lazy
/// arm is not being reached, whatever the reason.
pub fn demand_paging_report(t: &mut Suite) {
    use core::sync::atomic::Ordering;
    let n = crate::idt::USER_DEMAND_FAULTS.load(Ordering::Relaxed);
    t.note("mmap: user pages demand-paged from a region", n);
    t.check("mmap: the lazy path was actually taken", n > 0);
    // The file arm separately, because it is the one added on 2026-09-13 and the
    // one that goes quiet rather than red when it regresses. Noted, not
    // `check`ed: whether any program in the boot suite `mmap`s a **file** is a
    // property of the disk image, not of this kernel — in the guest that runs
    // `cargo`, every `ld.so` does, and these two numbers are what say the mapping
    // of a 300 MB shared object cost a few hundred pages instead of 76 000.
    t.note(
        "mmap: faults served from a file",
        FILE_DEMAND_FAULTS.load(Ordering::Relaxed),
    );
    t.note(
        "mmap: pages filled from a file (readahead included)",
        FILE_PAGES_FILLED.load(Ordering::Relaxed),
    );
}

#[cfg(not(feature = "no-tests"))]
/// Pin the VA placer against the shapes that matter: first fit, skip an
/// occupied range, reuse a hole, and refuse when the window cannot hold it.
///
/// A pure function over a `Vec<MmapRegion>`, so it needs no process and no
/// address space — which is what makes it testable at all. The eager/lazy
/// decision and the region algebra it sits between are host-tested in their own
/// crates; this is the one piece of `mmap` policy that is genuinely this
/// target's own.
fn va_placement_check(t: &mut Suite) {
    fn region(start: usize, pages: usize) -> MmapRegion {
        MmapRegion::inherited_with_prot(start, pages, Prot::RW_NO_EXEC)
    }
    const PG: usize = 4096;

    t.check_eq(
        "mmap va: an empty space places at the base",
        find_free_va(&mut [], 4).unwrap_or(0) as u64,
        MMAP_BASE as u64,
    );

    // One region at the base: the next mapping goes immediately after it, not
    // at some bumped-past address.
    let mut one = [region(MMAP_BASE, 4)];
    t.check_eq(
        "mmap va: the next mapping abuts the first",
        find_free_va(&mut one, 1).unwrap_or(0) as u64,
        (MMAP_BASE + 4 * PG) as u64,
    );

    // A hole between two regions is reused if the request fits, and skipped if
    // it does not. This is the whole difference from the bump allocator, both
    // directions asserted.
    let mut holed = [region(MMAP_BASE, 2), region(MMAP_BASE + 4 * PG, 2)];
    t.check_eq(
        "mmap va: a 2-page hole is reused by a 2-page request",
        find_free_va(&mut holed, 2).unwrap_or(0) as u64,
        (MMAP_BASE + 2 * PG) as u64,
    );
    t.check_eq(
        "mmap va: a 2-page hole is skipped by a 3-page request",
        find_free_va(&mut holed, 3).unwrap_or(0) as u64,
        (MMAP_BASE + 6 * PG) as u64,
    );

    // Order-independence. The list is not *kept* sorted — `detach` pushes
    // survivors onto the end — so the placer must give the same answer whatever
    // order it is handed the regions in. It gets there by sorting; this is the
    // case that says it actually does, rather than assuming its input.
    let mut reversed = [region(MMAP_BASE + 4 * PG, 2), region(MMAP_BASE, 2)];
    t.check_eq(
        "mmap va: the answer does not depend on region order",
        find_free_va(&mut reversed, 2).unwrap_or(0) as u64,
        (MMAP_BASE + 2 * PG) as u64,
    );

    // A region below the window must not drag the candidate backwards. Only
    // `MAP_FIXED` can make one, and `rustc`'s own image is exactly that shape.
    let mut low = [region(0x3010_0000, 4), region(MMAP_BASE, 2)];
    t.check_eq(
        "mmap va: a region under the base does not move the candidate",
        find_free_va(&mut low, 1).unwrap_or(0) as u64,
        (MMAP_BASE + 2 * PG) as u64,
    );

    // The shape that made this O(n²): a staircase of single-page regions with a
    // single-page hole between each pair, which is what a process `mmap`ing one
    // small allocation at a time accumulates. Handed in reversed, so the sort is
    // doing real work rather than confirming an order the builder produced.
    //
    // The assertion is on *work*, not on wall time — a boot suite cannot time
    // anything reliably, but it can demand the scan look at each region at most
    // once. Before the sort this walk cost N passes of N regions; the placement
    // it arrives at is the same one, which is the other half of the check.
    const N: usize = 1000;
    let mut staircase: Vec<MmapRegion> =
        (0..N).rev().map(|i| region(MMAP_BASE + i * 2 * PG, 1)).collect();
    let (placed, examined) = find_free_va_scan(&mut staircase, 2);
    t.check_eq(
        "mmap va: a 1000-region staircase places past the last step",
        placed.unwrap_or(0) as u64,
        (MMAP_BASE + (2 * N - 1) * PG) as u64,
    );
    t.check(
        "mmap va: the scan is one pass, not one pass per region",
        examined <= N,
    );

    // The window is finite and the refusal is `None`, not a wrapped address.
    t.check(
        "mmap va: a request larger than the window has no placement",
        find_free_va(&mut [], MMAP_VA_SPAN / PG + 1).is_none(),
    );
    t.check(
        "mmap va: a page count that would overflow has no placement",
        find_free_va(&mut [], usize::MAX).is_none(),
    );
}

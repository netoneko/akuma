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
//! A **writable `MAP_SHARED`** file mapping is served since 2026-09-22 — the
//! last refusal in this family. Writes reach the file by write-back on
//! `munmap`/`msync`/`MADV_DONTNEED`/exit over demand-paged pages. **Since
//! 2026-10-03 those pages are shared**: every mapper of a page maps one frame
//! from the shared writable page table (`akuma_exec::shm`,
//! `akuma-fpcache-rw`), which is what SQLite's multi-process WAL index needs —
//! the per-process copies before it corrupted goose's database. See
//! `docs/reference/subsystems/amd64-shared-write-mmap.md` for the design and
//! `docs/archive/AKUMA_AMD64_SHARED_WRITABLE_PAGES.md` for how it was found.
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

use crate::usermode;
use akuma_mmap::{FileBacking, MmapRegion, PhysFrame, Prot, insert_region_sorted};
#[cfg(not(feature = "no-tests"))]
use akuma_selftest::Suite;
use alloc::vec::Vec;

use crate::fd::errno;

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

/// This kernel's automatic-placement window. No hole: there is no identity
/// map in the lower half here (`phys.rs`).
const WINDOW: akuma_mmap::place::Window = akuma_mmap::place::Window { lo: MMAP_BASE, hi: MMAP_TOP, hole: 0..0 };

/// [`akuma_mmap::place::find_free_va_from`] over [`WINDOW`], in pages — the
/// one placer both kernels use, cursor and wrap included.
fn find_free_va_from(regions: &mut [MmapRegion], pages: usize, hint: usize) -> (Option<usize>, usize) {
    let Some(len) = pages.checked_mul(PAGE_SIZE as usize) else { return (None, 0) };
    akuma_mmap::place::find_free_va_from(regions, len, hint, &WINDOW)
}

/// The demand-grown part of the main stack: `max_pages` below `top`, of which
/// the top `eager_pages` are already mapped by `loader::build_stack`. An
/// anonymous RW region with no file, so `fault_in` zero-fills a page when the
/// stack reaches it. Placed under the process's mmap window by
/// `ProcessMemory`'s `mmap_limit` (`stack_bottom - 1 MiB`).
pub fn lazy_stack_region(top: u64, eager_pages: usize, max_pages: usize) -> MmapRegion {
    let base = top as usize - max_pages * PAGE_SIZE as usize;
    MmapRegion::inherited_with_prot(base, max_pages - eager_pages, Prot::RW_NO_EXEC)
}

/// [`lazy_stack_region`], inserted into the current process's region list — for
/// `execve`, which clears the list when it installs the new image.
pub fn register_lazy_stack(top: u64, eager_pages: usize, max_pages: usize) {
    let region = lazy_stack_region(top, eager_pages, max_pages);
    usermode::with_current_regions(|regions| insert_region_sorted(regions, region));
}

/// How many placements [`check_placement_free`] has inspected.
///
/// The positive control, and it is not optional: an instrument that reports
/// nothing has two readings — "no overlap happened" and "the check never ran" —
/// and a hunt that cannot tell them apart has learned nothing. Reported on the
/// fault line, which is the one place a failing run is guaranteed to reach.
pub static PLACEMENTS_CHECKED: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

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

/// `mmap` of `/dev/fb0`: map the boot framebuffer into ring 3,
/// **write-combining**, eagerly.
///
/// The bounds and flag rules are `akuma_fbdev::mmap_check` (host-tested):
/// `MAP_SHARED` only, page-aligned offset, inside the framebuffer, never
/// executable. Each page is `akuma_mmu::UserAddressSpace::map_device_page`
/// with `MemAttr::WriteCombine`, which sets the 4 KiB PTE's PAT bit — PAT entry
/// 4, programmed WC on every core by `multiboot2::map_wc` — so stores from the
/// program reach the screen at the same ~3 GB/s the console's own clear does,
/// not the 71 MB/s an uncached mapping gives.
///
/// Eager because there is nothing to fault from: the pages exist, and a
/// 3840x2160x4 screen is 8100 PTEs. The region is recorded like any other so
/// `munmap`, `mprotect` and `fork` find it; the frames are **not** in the
/// ledger and carry no share count, which is what keeps every teardown path
/// from handing device memory to the page allocator (`unmap_range` frees only
/// what the ledger tracks; `fork` maps a non-write-back leaf verbatim).
pub fn mmap_framebuffer(addr: u64, len: u64, prot: u32, flags: u32, offset: u64) -> u64 {
    use akuma_syscalls_linux::flags::map::{MAP_FIXED, MAP_SHARED};
    let Some(g) = crate::fbdev::geometry() else { return errno::ENODEV };
    let shared = flags & MAP_SHARED != 0;
    let pages = match akuma_fbdev::mmap_check(len, offset, shared, prot & PROT_EXEC != 0, &g) {
        Ok(p) => p as usize,
        Err(akuma_fbdev::MapRefusal::Exec) => return errno::EACCES,
        Err(akuma_fbdev::MapRefusal::Invalid) => return errno::EINVAL,
    };
    if !have_address_space() {
        return errno::ESRCH;
    }
    let byte_len = pages * PAGE_SIZE as usize;
    let base = if flags & MAP_FIXED != 0 {
        let want = addr as usize;
        if want.saturating_add(byte_len) > USER_VA_LIMIT {
            return errno::EINVAL;
        }
        unmap_range(want, want + byte_len);
        want
    } else {
        let Some(found) = usermode::with_current_regions_and_cursor(|regions, cursor| {
            let hint = cursor.load(core::sync::atomic::Ordering::Relaxed);
            let placed = find_free_va_from(regions, pages, hint).0;
            if let Some(base) = placed {
                cursor.store(base.saturating_add(byte_len), core::sync::atomic::Ordering::Relaxed);
            }
            placed
        })
        .flatten() else {
            return errno::ENOMEM;
        };
        found
    };
    let region_prot = Prot::from_prot(prot);
    usermode::with_current_regions(|regions| {
        insert_region_sorted(regions, MmapRegion::inherited_with_prot(base, pages, region_prot));
    });
    let pte = PteProt::from_region(region_prot);
    let phys = (g.phys + offset) as usize;
    let mapped = usermode::with_current_address_space(|uas| {
        (0..pages).all(|i| {
            let off = i * PAGE_SIZE as usize;
            uas.map_device_page(base + off, phys + off, pte, akuma_mmu::MemAttr::WriteCombine)
        })
    })
    .unwrap_or(false);
    if !mapped {
        // Out of page-table frames partway: undo all of it. The device pages
        // are untracked, so the unmap frees nothing but page tables' contents.
        unmap_range(base, base + byte_len);
        return errno::ENOMEM;
    }
    base as u64
}

/// Allocate, zero, map and record one anonymous page at `va`.
///
/// The single place a frame becomes part of a user address space on this path,
/// which is why the ledger update is here and not at the three call sites.
/// A frame the ledger does not know about is a frame `Process::free` will not
/// release — the leak that made every post-`fork` `mmap` permanent.
fn populate_page(va: usize, prot: Prot) -> bool {
    // Zeroed before mapping: a recycled frame otherwise hands ring 3 whatever the
    // previous owner left in it. The same rule the ELF loader follows.
    let Some(frame) = akuma_pmm::alloc_page_zeroed() else {
        note_fault_refusal(va as u64, "oom");
        return false;
    };
    // A brand-new frame is unshared, so `pte_prot_for`'s CoW rule cannot fire;
    // it is used anyway so there is one answer to "what bits does this region
    // get" rather than two that could drift.
    let (pte, cow) = pte_prot_for(prot, frame);
    match install_filled_page(va, PhysFrame::new(frame), pte, cow) {
        PageInstall::Mapped => true,
        // A peer core filled this page while this fault was in flight. Its
        // frame holds the real contents; give ours back and report the fault
        // served, because it is — the instruction re-executes against a page
        // that is present, which is all it was waiting for.
        PageInstall::Raced => {
            akuma_pmm::free_page(frame, 0);
            note_fault_race(va);
            true
        }
        // Out of page-table memory, or no process. Unchanged.
        PageInstall::Failed => {
            note_fault_refusal(va as u64, "install");
            akuma_pmm::free_page(frame, 0);
            false
        }
    }
}

/// What [`install_filled_page`] decided.
///
/// Three outcomes, because "a peer core got there first" is a **success** with
/// a frame to give back and not a failure — a caller that treats it as failure
/// turns a served fault into a `SIGSEGV`.
enum PageInstall {
    /// This frame is now the page at `va`.
    Mapped,
    /// A page was already present at `va`; nothing was changed and the caller
    /// still owns its frame.
    Raced,
    /// Nothing was mapped: no address space, or no memory for a page table.
    Failed,
}

/// Publish a freshly filled frame at `va` — **unless something is already
/// there**.
///
/// # The presence test is the point, and it has to be inside the hold
///
/// [`akuma_mmu::UserAddressSpace::map_and_track_pte`] overwrites a present leaf
/// without asking. So a second fault on a page a peer core has already filled
/// does not merely waste a frame: it replaces the live translation, and the data
/// the process wrote through the first one is gone. For an anonymous page the
/// replacement is *zeroed*, which is how the damage presents — a heap word that
/// reads back as 0.
///
/// Nothing reports it. The orphaned frame is still tracked by the ledger and
/// still refcounted, so the PMM's UAF quarantine and its premature-free check
/// both stay silent; a `pmm-forensics` build of this kernel ran the failing
/// build to completion without a single complaint while the corruption happened
/// (`docs/archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §14).
///
/// Two cores really do take that fault. The page is absent when each of them
/// traps, and the BKL serialises only the *servicing*: both faults are
/// delivered, then serviced one after the other, and the second arrives at a
/// page that became present in between. `fill_file_pages` has always known this
/// — "A peer filled the very page this fault is about. Present is present" — and
/// tested `is_current_user_range_mapped` before filling. That test is **outside**
/// any hold, so it narrows the window rather than closing it; this closes it, by
/// making the test and the map one critical section against a peer doing the
/// same thing. Same serialisation, and the same reason, as the owner's lock
/// `cow_write_fault` takes.
///
/// The fill stays outside the hold at every caller — a file read takes the
/// descriptor table, and taking that underneath the address-space lock would be
/// the one place in this module where the two are ordered that way.
fn install_filled_page(va: usize, frame: PhysFrame, pte: PteProt, cow: bool) -> PageInstall {
    usermode::with_current_address_space(|uas| {
        if uas.pte_prot(va).is_some() {
            PageInstall::Raced
        } else if uas.map_and_track_pte(va, frame, pte, cow) {
            PageInstall::Mapped
        } else {
            PageInstall::Failed
        }
    })
    .unwrap_or(PageInstall::Failed)
}

/// [`install_filled_page`] for the fill paths that hand a frame back, plus the
/// frame's disposal on the two outcomes where it is no longer wanted.
///
/// The [`FilledPage`] distinction is load-bearing at exactly one call site:
/// `fill_file_pages` must not put a frame it no longer owns into the shared
/// file-page cache (so `Raced` is not `Mapped`) and must not stop reading ahead
/// because a peer beat it to one page (so `Raced` is not `Failed`).
fn install_or_release(
    va: usize,
    frame: usize,
    pte: PteProt,
    cow: bool,
) -> FilledPage {
    match install_filled_page(va, PhysFrame::new(frame), pte, cow) {
        PageInstall::Mapped => FilledPage::Mapped(PhysFrame::new(frame)),
        PageInstall::Raced => {
            akuma_pmm::free_page(frame, 0);
            note_fault_race(va);
            FilledPage::Raced
        }
        PageInstall::Failed => {
            akuma_pmm::free_page(frame, 0);
            FilledPage::Failed
        }
    }
}

/// What a file fill did with its page.
///
/// `Raced` is deliberately not folded into either of the others: it is a
/// **success** (the page is present) whose frame the caller no longer owns (so
/// it must not be published to the file-page cache). Collapsing it into
/// `Mapped` would insert a freed frame into the cache; collapsing it into
/// `Failed` would turn a served fault into a `SIGSEGV`.
enum FilledPage {
    Mapped(PhysFrame),
    Raced,
    Failed,
}

/// File-mapping page fills that got fewer bytes than the file was known to
/// hold — see the call site for why that is never legitimate.
///
/// Steady-state zero. A non-zero reading is wrong bytes in a mapped file page,
/// which is the quietest failure this module has: nothing errors, nothing
/// faults, and the first thing anyone sees is a compiler failing to find a
/// symbol in a crate it just read.
pub static FILE_FILL_SHORT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Count one short fill, and say so on the first and every 1024th.
fn note_fill_short(va: usize, want: usize, got: usize) {
    let n = FILE_FILL_SHORT.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
    if n == 1 || n.is_multiple_of(1024) {
        let mut w = akuma_primitives::console::StackWriter::<128>::new();
        let _ = core::fmt::write(
            &mut w,
            format_args!("[FILL-SHORT] #{n} va=0x{va:x} want={want} got={got}\n"),
        );
        w.flush();
    }
}

/// Demand faults that arrived at a page a peer core had already filled.
///
/// Anonymous **and** file-backed — every caller of [`install_filled_page`]
/// bumps it, because the hazard is the same one at all of them.
///
/// Kept permanently, and reported by [`demand_paging_report`], because the
/// quantity is the evidence: this counter is the population that used to have
/// its page silently replaced (with zeros, on the anonymous path), and a build
/// that reports zero of them has not proved the race is gone so much as failed
/// to run into it. Same argument as [`FILE_PAGES_SHARED`] — an arm that is
/// never taken is invisible in every other measurement.
pub static PAGE_FAULT_RACES: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Count one, and say so on the first and then every 4096th.
///
/// The counter alone is not enough evidence and the reason is specific: the
/// boot suite reports it, the boot suite is not concurrent, and it therefore
/// reports **0** on a kernel where the race fires thousands of times under the
/// load that matters. A run that only ever sees that 0 cannot tell "the guard
/// is working" from "the guard is dead code", which is the difference the whole
/// fix turns on. A line on the console can be read after a `cargo` build, by
/// `dmesg` or off the host-side log, without a fault to carry it.
///
/// Milestones rather than every occurrence: it fires per page, and a line per
/// page would be its own denial of service on the console.
fn note_fault_race(va: usize) {
    let n = PAGE_FAULT_RACES.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
    if n == 1 || n.is_multiple_of(4096) {
        let mut w = akuma_primitives::console::StackWriter::<128>::new();
        let _ = core::fmt::write(
            &mut w,
            format_args!("[MM] fault race #{n} (va=0x{va:x}) — page already served by a peer\n"),
        );
        w.flush();
    }
}

/// Demand faults the kernel **refused to serve** — the population that reaches
/// ring 3 as a `SIGSEGV`.
///
/// Zero in a healthy kernel: every not-present fault inside a live region is
/// servable, so a non-zero reading here is the fork-vs-threads SIGSEGV's exact
/// birth record. Kept permanently and printed on the first and every 1024th,
/// because the crash that makes the process die also takes the console with it —
/// a line already on the UART is the only evidence that survives.
///
/// `why` names the arm that refused, so the print says not just *that* a fault
/// was refused but *which* decision produced the refusal:
/// `no-region` (no mapping covered the address), `prot-none` (a reservation or
/// guard page), `oom` (no frame), `as-none` (no address space), `install`
/// (the publish into the page tables failed), `cow-fault` (`akuma-cow` decided
/// the write was a genuine protection violation).
pub static FAULT_REFUSALS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Count one refusal, and say why on the first and every 1024th.
pub fn note_fault_refusal(va: u64, why: &str) {
    let n = FAULT_REFUSALS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
    if n <= 8 || n.is_multiple_of(1024) {
        let mut w = akuma_primitives::console::StackWriter::<160>::new();
        let _ = core::fmt::write(
            &mut w,
            format_args!("[MM] fault REFUSED #{n} va=0x{va:x} why={why}\n"),
        );
        w.flush();
    }
}

/// Allocate, **fill from an already-read buffer**, map and record one page.
///
/// [`populate_file_page`]'s batched twin — [`sys_mmap`]'s fill loop reads a
/// 64 KiB chunk once and hands each 4 KiB slice here, so ext2's per-call fixed
/// cost is paid per chunk instead of per page. Same rules: freshly allocated
/// frame (no stale contents), PTE edit and ledger entry under the
/// address-space hold, frame freed on a failed map.
fn populate_file_page_from(va: usize, prot: Prot, src: &[u8]) -> FilledPage {
    // Zeroed by the allocator, so the tail past `src` is already right.
    let Some(frame) = akuma_pmm::alloc_page_zeroed() else {
        return FilledPage::Failed;
    };
    // Clamped rather than trusted: the caller slices a chunk buffer, and a page
    // is the most this frame can hold. A longer `src` is a caller bug; the
    // copy is bounds-checked, so it is clamped here instead of refused.
    let n = src.len().min(PAGE_SIZE as usize);
    if !akuma_mmu::copy_to_phys(frame, &src[..n]) {
        akuma_pmm::free_page(frame, 0);
        return FilledPage::Failed;
    }
    let (pte, cow) = pte_prot_for(prot, frame);
    install_or_release(va, frame, pte, cow)
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
        // Binary search, not a scan: this runs once per **page fault**, and a
        // `rustc` mid-build holds ~4 400 regions and takes ~86 000 faults
        // compiling one small crate. It is also the one hot walk `[PSTATS]`
        // cannot see, because faults are counted there but never timed.
        // Relies on the sorted invariant `akuma_mmap::insert_region_sorted`
        // documents and every mutator in that crate preserves.
        let idx = akuma_mmap::region_index_containing(regions, page);
        let Some(idx) = idx else {
            note_fault_refusal(addr, "no-region");
            return None;
        };
        let region = &regions[idx];
        let prot = region.recorded_prot().unwrap_or(Prot::RW_NO_EXEC);
        if prot.is_none() {
            note_fault_refusal(addr, "prot-none");
            return None; // a reservation, or a guard page: a real fault
        }
        // Extent as well as identity: the readahead below must not run off the
        // end of the region it started in. And whether the pages are shared
        // **writable** by identity — a lazy region with a write-back record —
        // which is a different table and a different fill.
        Some((
            prot,
            region.file.map(|f| (f, region.start_va, region.pages, region.shared_write.is_some())),
        ))
    })
    .flatten() else {
        return false;
    };

    match file {
        None => {
            let ok = populate_page(page, prot);
            if !ok {
                note_fault_refusal(addr, "populate");
            }
            ok
        }
        Some((file, region_start, region_pages, shared_write)) => {
            let t0 = akuma_cpu::tsc::rdtsc();
            let ok = if shared_write {
                fill_shared_write_pages(page, prot, file, region_start, region_pages)
            } else {
                fill_file_pages(page, prot, file, region_start, region_pages)
            };
            let t1 = akuma_cpu::tsc::rdtsc();
            FILE_FAULT_CYCLES.fetch_add(t1.wrapping_sub(t0), core::sync::atomic::Ordering::Relaxed);
            ok
        }
    }
}

/// TSC cycles spent in [`fill_file_pages`] on the demand-fault path; with
/// [`FILE_DEMAND_FAULTS`] this splits `[FAULTSTAT]`'s service time into
/// file-backed and anonymous.
pub static FILE_FAULT_CYCLES: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

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
    // amortization the eager fill does, for the same reason — **loaded on the
    // first page that misses the shared cache, not up front.** It used to run
    // before the loop on every file fault, and on a `-j4` self-host build 95 % of
    // the pages filled are cache hits that never touch it: 510 k file faults at
    // ~38 us each (93 % of all fault service time, `[FAULTSTAT]`) were mostly an
    // allocate + zero + ext2 read of 64 KiB that the loop then ignored. A window
    // that is all hits now costs no buffer and no read. A failed allocation is
    // not a failed fault: the per-page path below reads straight into each frame
    // and needs no buffer at all.
    let mut buf: Option<Vec<u8>> = None;
    let mut buf_tried = false;

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

        // A page some process maps **shared and writable** lives in the shared
        // writable table until its writer flushes, and the file (and the file
        // page cache above it) is stale until then. A read-only mapping of the
        // same file must see the table's frame, as Linux's one page cache
        // would give it: Chromium's font service fills a shared-memory region
        // through a writable mapping and hands the renderer a read-only one,
        // and the renderer used to read the file's zeros — no system font ever
        // loaded (`shmregionprobe.c`, ryzen 2026-10-08). `active()` keeps this
        // to one atomic load when no process maps a file shared-writable.
        if !region_pte.write
            && let Some(owner) = usermode::current_mm_process()
        {
            let key = akuma_fpcache_rw::Key::new(file.mount_id, file.inode, offset);
            if let Some(ok) = akuma_exec::shm::map_existing(owner, va, akuma_exec::mmu::user_flags::to_pte(prot), key) {
                if ok {
                    FILE_PAGES_FILLED.fetch_add(1, Ordering::Relaxed);
                }
                if va == page {
                    faulting_page_ok = ok;
                }
                if !ok {
                    break;
                }
                continue;
            }
        }

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
            let ok = map_shared_file_page(va, prot, frame, false);
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

        if !buf_tried {
            buf_tried = true;
            let want = (last - first) * PAGE_SIZE as usize;
            let mut v: Vec<u8> = Vec::new();
            if v.try_reserve(want).is_ok() {
                v.resize(want, 0);
                let (window_offset, _) = file.page_source(first);
                match crate::fd::file_bytes_by_inode(file.mount_id, file.inode, window_offset, &mut v) {
                    // The file is gone, or was never readable by inode. Fall back
                    // to the per-page path, which fails the same way and one page
                    // at a time.
                    None => {}
                    // Short at EOF, or nothing at all: the rest of the window is
                    // zero-fill, and the buffer must say so rather than keep
                    // whatever `resize` left there.
                    Some(n) => {
                        v[n..].fill(0);
                        buf = Some(v);
                    }
                }
            }
        }
        let filled = match buf.as_ref() {
            Some(b) => {
                let at = (idx - first) * PAGE_SIZE as usize;
                populate_file_page_from(va, prot, &b[at..at + from_file])
            }
            None => populate_file_page_by_inode(va, prot, file, offset, from_file),
        };
        if let FilledPage::Mapped(frame) = filled {
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
        // `Raced` is a success with nothing to publish: the page is present,
        // but this address space no longer owns the frame, so it must not go
        // into the shared cache above (that is why the `if let` is on `Mapped`
        // alone) and it must not be reported as a failure here.
        let ok = !matches!(filled, FilledPage::Failed);
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
fn map_shared_file_page(va: usize, prot: Prot, frame: PhysFrame, identity: bool) -> bool {
    // A read-only cached page goes through `pte_prot_for` like any other (its
    // share count is always > 0, so a writable region gets it CoW-marked). A
    // page of the shared **writable** table is shared by identity: mapped as
    // the region says, writable, never marked — a write that broke "sharing"
    // here would be the incoherence that table exists to remove.
    let (pte, cow) = if identity {
        (PteProt::from_region(prot), false)
    } else {
        pte_prot_for(prot, frame.addr)
    };
    // `(mapped, release, raced)`. `release` says the cache's reference is
    // surplus and must be given back; `raced` is reported outside the hold.
    let outcome = usermode::with_current_address_space(|uas| {
        // **A present leaf is never overwritten** — [`install_filled_page`] has
        // the argument, and it applies here with one extra consequence: this
        // path arrives holding a *file-page cache* reference, so overwriting
        // would strand the reference belonging to whatever was mapped before.
        if uas.pte_prot(va).is_some() {
            return (true, true, true);
        }
        // `true` — the caller's reference. `adopt_user_frame` reports it back as
        // *surplus* when this address space already held the frame at another
        // VA, because teardown frees each distinct frame exactly once and a
        // second reference for a second VA would never be balanced.
        let surplus = uas.adopt_user_frame(frame, true);
        if uas.map_page_pte(va, frame.addr, pte, cow) {
            (true, surplus, false)
        } else {
            // Nothing was mapped, so this address space is not a mapper: undo
            // the adoption and hand the reference back.
            let _ = uas.remove_user_frame(frame);
            (false, true, false)
        }
    });
    match outcome {
        // No address space at all — a kernel thread has no business faulting
        // into one, and the reference must not be kept for it.
        None => {
            akuma_pmm::free_page(frame.addr, 0);
            false
        }
        Some((mapped, release, raced)) => {
            if release {
                akuma_pmm::free_page(frame.addr, 0);
            }
            if raced {
                note_fault_race(va);
            }
            mapped
        }
    }
}

/// [`fill_file_pages`] for a **writable `MAP_SHARED`** region: every page comes
/// from, or goes into, the shared writable page table (`akuma_exec::shm`), so
/// every process mapping the file maps the same frame (2026-10-03).
///
/// Same window and the same "present is present" rule as the read-only fill.
/// Different in what a page *is*: it is filled from the file's **current**
/// bytes — the whole page, not clamped by the mapping's `filesz`, which is the
/// EOF at `mmap` time and wrong for a file that grew since (SQLite grows its
/// `-shm` and maps the new part in a later `mmap`) — and it is published
/// before it is mapped, so a peer faulting the same page lands on this frame
/// rather than filling its own.
fn fill_shared_write_pages(
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
    let mut faulting_page_ok = false;
    for idx in first..last {
        let va = region_start + idx * PAGE_SIZE as usize;
        if akuma_mmu::is_current_user_range_mapped(va, 1) {
            if va == page {
                faulting_page_ok = true;
            }
            continue;
        }
        let (offset, _) = file.page_source(idx);
        let key = akuma_fpcache_rw::Key::new(file.mount_id, file.inode, offset);
        let ok = shared_write_page(va, prot, key);
        if ok {
            FILE_PAGES_FILLED.fetch_add(1, Ordering::Relaxed);
        }
        if va == page {
            faulting_page_ok = ok;
        }
        if !ok {
            break;
        }
    }
    faulting_page_ok
}

/// Page `key` at `va`, from the shared writable page table: `akuma_exec::shm`,
/// the one implementation both kernels fault through.
fn shared_write_page(va: usize, prot: Prot, key: akuma_fpcache_rw::Key) -> bool {
    usermode::current_mm_process()
        .is_some_and(|p| akuma_exec::shm::fault_page(p, va, akuma_exec::mmu::user_flags::to_pte(prot), key))
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
) -> FilledPage {
    let Some(frame) = akuma_pmm::alloc_page() else {
        return FilledPage::Failed;
    };
    let want = from_file.min(PAGE_SIZE as usize);
    // The frame is unpublished until it is mapped below, so the view is the only
    // accessor for its duration.
    let read = akuma_mmu::with_phys_bytes_mut(frame, 0, PAGE_SIZE as usize, |page| {
        // Zeroed before the fill, never instead of it: zero is the value of a
        // byte past EOF and of nothing else.
        page.fill(0);
        if want == 0 {
            return Some(0);
        }
        crate::fd::file_bytes_by_inode(file.mount_id, file.inode, offset, &mut page[..want])
    })
    .flatten();
    if want > 0 {
        match read {
            None => {
                akuma_pmm::free_page(frame, 0);
                return FilledPage::Failed;
            }
            // **A short read here is never legitimate.** `want` is already
            // clamped to what the *file* has from this offset
            // (`FileBacking::filesz` via `page_source`), so unlike the
            // whole-page reads elsewhere in this module, "past EOF" is not an
            // explanation: the bytes exist and the filesystem did not hand them
            // over. The page keeps its zeros for the shortfall and the mapping
            // silently serves them, which is the `[FILL-SHORT]` failure the
            // AArch64 kernel tripwires for the same reason
            // (`docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` §12-§15: it presents as
            // `rustc` failing to find something in a dependency's metadata, not
            // as an I/O error). Reported, not made fatal — a `SIGSEGV` here
            // would replace wrong bytes with a dead build, and the counter is
            // what says whether it happens at all.
            Some(got) if got < want => note_fill_short(va, want, got),
            Some(_) => {}
        }
    }
    let (pte, cow) = pte_prot_for(prot, frame);
    install_or_release(va, frame, pte, cow)
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

/// Retire `[start, end)` of the current process: glue's `munmap` body
/// (`akuma_syscalls_glue::mem::unmap_current`), which reaches back into this
/// kernel only through [`release_shared_write`].
fn unmap_range(start: usize, end: usize) {
    akuma_syscalls_glue::mem::unmap_current(start, end);
}

/// `mmap` as ring 3 reaches it: glue's.
#[cfg(not(feature = "no-tests"))]
fn mmap_glue(addr: u64, len: u64, prot: u64, flags: u64, fd: u64, offset: u64) -> u64 {
    crate::usermode::to_glue(akuma_syscalls_abi::Syscall::Mmap, [addr, len, prot, flags, fd, offset])
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

    t.check_eq("mmap: zero length is EINVAL", mmap_glue(0, 0, 3, ANON, NO_FD, 0), errno::EINVAL);
    t.check_eq(
        "mmap: a length past the user half is ENOMEM",
        mmap_glue(0, USER_VA_LIMIT as u64, 3, ANON, NO_FD, 0),
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
    // Writable `MAP_SHARED` on a file is **servable** since 2026-09-22 — eager
    // fill + `MmapRegion::shared_write` + write-back on `munmap`/`msync`
    // (the refusal this used to assert is gone). What remains decidable from
    // the arguments with no open file is the refusal of a shared-writable
    // request on a descriptor that cannot be a write identity: same fd 5, no
    // process, `EACCES` from the identity probe before anything is placed.
    // The descriptor rules (not a file, not writable, no write identity:
    // `EACCES`) need the caller's fd table, so with no process they answer
    // `ESRCH`; `mem_suite`'s probes cover them from ring 3.
    t.check_eq(
        "mmap: a writable MAP_SHARED file mapping with no process is ESRCH",
        mmap_glue(0, 4096, u64::from(PROT_WRITE) | 1, MAP_SHARED, 5, 0),
        errno::ESRCH,
    );
    t.check_eq(
        "mmap: msync is served, not ENOSYS",
        crate::usermode::to_glue(akuma_syscalls_abi::Syscall::Msync, [0x1000_0000, 4096, 0, 0, 0, 0]),
        errno::ESRCH,
    );
    t.check_eq(
        "mmap: a file mapping at an unaligned offset is EINVAL",
        mmap_glue(0, 4096, 1, 0, 5, 1),
        errno::EINVAL,
    );
    // The descriptor probe. Nothing is open here, so fd 5 is not a regular file
    // and the request is refused **before** a frame is allocated — which is what
    // stops the zero-filled-file failure the old blanket refusal guarded
    // against. `populate_file_page` refuses the same thing a second time, at the
    // point where the zeros would otherwise be written.
    t.check_eq(
        "mmap: a file mapping with no process is ESRCH (the fd is asked after)",
        mmap_glue(0, 4096, 1, 0, 5, 0),
        errno::ESRCH,
    );
    // Granted since 2026-10-08 (V8's code range), so in the boot suite — no
    // current process — it must reach the address-space step and say so.
    t.check_eq(
        "mmap: writable+executable is accepted (ESRCH here: no process, not EINVAL)",
        mmap_glue(0, 4096, u64::from(PROT_WRITE | PROT_EXEC), ANON, NO_FD, 0),
        errno::ESRCH,
    );
    t.check_eq(
        "mmap: an unaligned MAP_FIXED address is EINVAL",
        mmap_glue(0x5000_1001, 4096, 3, ANON | u64::from(MAP_FIXED), NO_FD, 0),
        errno::EINVAL,
    );
    t.check_eq(
        "mmap: MAP_FIXED over the kernel half is EINVAL",
        mmap_glue(USER_VA_LIMIT as u64, 4096, 3, ANON | u64::from(MAP_FIXED), NO_FD, 0),
        errno::EINVAL,
    );
    t.check_eq(
        "munmap: an unaligned address is EINVAL",
        crate::usermode::to_glue(akuma_syscalls_abi::Syscall::Munmap, [0x1001, 4096, 0, 0, 0, 0]),
        errno::EINVAL,
    );
    t.check_eq(
        "munmap: a range into the kernel half is EINVAL",
        crate::usermode::to_glue(akuma_syscalls_abi::Syscall::Munmap, [(USER_VA_LIMIT - 4096) as u64, 8192, 0, 0, 0, 0]),
        errno::EINVAL,
    );
    t.check_eq(
        "munmap: no process is ESRCH",
        crate::usermode::to_glue(akuma_syscalls_abi::Syscall::Munmap, [0x1_0000_0000, 4096, 0, 0, 0, 0]),
        errno::ESRCH,
    );
    t.check_eq(
        "mprotect: writable+executable is accepted (ESRCH here: no process, not EINVAL)",
        crate::usermode::to_glue(
            akuma_syscalls_abi::Syscall::Mprotect,
            [0x1_0000_0000, 4096, u64::from(PROT_WRITE | PROT_EXEC), 0, 0, 0],
        ),
        errno::ESRCH,
    );
    t.check_eq(
        "mprotect: a range into the kernel half is EINVAL, not a walk of it",
        crate::usermode::to_glue(
            akuma_syscalls_abi::Syscall::Mprotect,
            [(USER_VA_LIMIT - 4096) as u64, 8192, 3, 0, 0, 0],
        ),
        errno::EINVAL,
    );

    // Every one of these runs during the boot self-tests, where there is no
    // current process — so each must have been refused *before* reaching the
    // region list. A slotted caller is what `ESRCH` distinguishes, and a
    // refusal that returned it here would mean the ordering had slipped.
    t.check_eq(
        "mmap: a valid anonymous request with no process is ESRCH, not a mapping",
        mmap_glue(0, 4096, 3, ANON, NO_FD, 0),
        errno::ESRCH,
    );

    madvise_check(t);
    mremap_check(t);
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
    fn sys_mremap(old: u64, old_size: u64, new_size: u64, flags: u64) -> u64 {
        crate::usermode::to_glue(akuma_syscalls_abi::Syscall::Mremap, [old, old_size, new_size, flags, 0, 0])
    }
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
    // Refused, not silently placed elsewhere — see glue `sys_mremap`'s comment.
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
    fn sys_madvise(addr: u64, len: u64, advice: u64) -> u64 {
        crate::usermode::to_glue(akuma_syscalls_abi::Syscall::Madvise, [addr, len, advice, 0, 0, 0])
    }
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
    // Advisory: with no process there is nothing to pre-fault.
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
/// After the boot suite has run real programs: how many pages the lazy path served.
///
/// Noted, not checked: since `brk` grows (2026-11-12) the suite's static musl
/// programs take their heap from it and need no lazy mapping, so zero here is
/// not a regression. `scripts/mem_suite.py` (`mmaplazy`, `mmap_stress`) is what
/// proves the lazy path.
pub fn demand_paging_report(t: &mut Suite) {
    use core::sync::atomic::Ordering;
    let n = crate::idt::USER_DEMAND_FAULTS.load(Ordering::Relaxed);
    t.note("mmap: user pages demand-paged from a region", n);
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
    // The double-fault race [`populate_page`] closes. Noted rather than
    // `check`ed in either direction: a boot suite is not concurrent enough to
    // guarantee a hit, and a zero here means "this boot did not race", not
    // "the guard is unnecessary". On a `cargo -j2` build at `SMP>=2` it is
    // thousands.
    t.note(
        "mmap: demand faults a peer had already served",
        PAGE_FAULT_RACES.load(Ordering::Relaxed),
    );
    // Unlike the race counter above, this one is `check`ed: a short fill is
    // never legitimate at this call site (see `note_fill_short`), it needs no
    // concurrency to happen, and the boot suite maps real files.
    t.check(
        "mmap: no file page was filled short",
        FILE_FILL_SHORT.load(Ordering::Relaxed) == 0,
    );
}

/// Point the calling process's `[start, start + len)` at the contiguous physical block at `pa`.
///
/// The hypervisor's "direct memory transfer" for a VMM's guest RAM (the hypervisor driver's `kvm::api`,
/// `docs/archive/AKUMA_FIRECRACKER_KVM.md`). A page already present keeps its bytes (copied into the
/// block) and its old frame goes back through the ledger exactly as `munmap` would; an absent page
/// becomes the block's zeroed page. The new leaves are **borrowed**, like `mmap_framebuffer`'s: not in
/// the ledger, no share count, so `munmap`/exit/`fork` never hand the block to the page allocator.
///
/// `start`/`len`/`pa` are page aligned. `false`: out of page-table frames partway (what was moved stays
/// moved, and consistent). Only the calling thread's TLB is flushed: call before the VMM has other
/// threads touching the range.
#[must_use]
pub fn remap_user_to_phys(start: usize, len: usize, pa: usize) -> bool {
    use akuma_mmu::MemAttr;
    let page = PAGE_SIZE as usize;
    if len == 0 || !start.is_multiple_of(page) || !len.is_multiple_of(page) || !pa.is_multiple_of(page) {
        return false;
    }
    let end = start + len;
    let pte = PteProt::from_region(Prot::from_prot(0b11));
    usermode::with_current_address_space(|uas| {
        let mut buf = [0u8; PAGE_SIZE as usize];
        uas.rewrite_leaves_in_range(start, end, |ledger, leaf| {
            if leaf.attr != MemAttr::WriteBack {
                return LeafAction::Keep;
            }
            let new_pa = pa + (leaf.va - start);
            if !akuma_mmu::copy_from_phys(leaf.pa, &mut buf) || !akuma_mmu::copy_to_phys(new_pa, &buf) {
                return LeafAction::Keep;
            }
            if ledger.remove_user_frame(PhysFrame::new(leaf.pa)) && akuma_pmm::cow_ref_dec(leaf.pa) {
                akuma_pmm::free_page(leaf.pa, 0);
            }
            LeafAction::Remap(new_pa, pte, false)
        });
        (0..len / page).all(|i| {
            let va = start + i * page;
            uas.translate(va).is_some() || uas.map_device_page(va, pa + i * page, pte, MemAttr::WriteBack)
        })
    })
    .unwrap_or(false)
}

/// Map the physical frames `frames` (page-aligned addresses, in order) into the calling process.
///
/// One read-write shared region, at `addr` with `MAP_FIXED` or at a fresh address. The frames are
/// **borrowed**, as `mmap_framebuffer`'s are: recorded as a region, absent from the ledger, so no
/// teardown path hands them to the allocator. For the hypervisor's vCPU run page (the hypervisor driver's `kvm::api`), which
/// the kernel keeps writing through the physmap while the VMM reads it. Returns the address or an errno.
#[must_use]
pub fn mmap_frames(addr: u64, flags: u32, prot: u32, frames: &[usize]) -> u64 {
    use akuma_syscalls_linux::flags::map::MAP_FIXED;
    let pages = frames.len();
    if pages == 0 {
        return errno::EINVAL;
    }
    if !have_address_space() {
        return errno::ESRCH;
    }
    let byte_len = pages * PAGE_SIZE as usize;
    let base = if flags & MAP_FIXED != 0 {
        let want = addr as usize;
        if want.saturating_add(byte_len) > USER_VA_LIMIT {
            return errno::EINVAL;
        }
        unmap_range(want, want + byte_len);
        want
    } else {
        let Some(found) = usermode::with_current_regions_and_cursor(|regions, cursor| {
            let hint = cursor.load(core::sync::atomic::Ordering::Relaxed);
            let placed = find_free_va_from(regions, pages, hint).0;
            if let Some(base) = placed {
                cursor.store(base.saturating_add(byte_len), core::sync::atomic::Ordering::Relaxed);
            }
            placed
        })
        .flatten() else {
            return errno::ENOMEM;
        };
        found
    };
    let region_prot = Prot::from_prot(prot);
    usermode::with_current_regions(|regions| {
        insert_region_sorted(regions, MmapRegion::inherited_with_prot(base, pages, region_prot));
    });
    let pte = PteProt::from_region(region_prot);
    let mapped = usermode::with_current_address_space(|uas| {
        frames.iter().enumerate().all(|(i, &pa)| {
            uas.map_device_page(base + i * PAGE_SIZE as usize, pa, pte, akuma_mmu::MemAttr::WriteBack)
        })
    })
    .unwrap_or(false);
    if !mapped {
        unmap_range(base, base + byte_len);
        return errno::ENOMEM;
    }
    base as u64
}

//! Stage A: bring up the kernel heap and the physical frame allocator on x86_64.
//!
//! Both `akuma-alloc` and `akuma-pmm` are architecture-neutral and built for
//! `x86_64-unknown-none` before this target existed. This module is the test of
//! that: if the claim in `docs/archive/REDUCING_PLATFORM_DEPENDENCY.md` is right,
//! wiring them up needs no new arch code at all — only a memory map, which PVH
//! hands over (`crate::hvm`).
//!
//! # Ordering, which is not obvious and not optional
//!
//! **The heap must come up before the PMM.** `akuma_pmm`'s `init` allocates its
//! own free-page bitmap with `alloc::vec![0u64; n]`, so a PMM initialised before
//! there is a heap faults inside the allocator. The dependency runs the opposite
//! way to the intuition that a frame allocator is the more primitive thing.
//!
//! That forces the layout below: the heap is carved statically out of the region
//! immediately above the kernel image, and the PMM is then told that everything
//! up to the end of that heap is already spoken for.
//!
//! ```text
//!   0x100000            0x200000        _kernel_end   heap_end        ram_end
//!   |  low RAM          |  kernel image |  heap       |  PMM frames        |
//!   +-------------------+---------------+-------------+--------------------+
//!   \____________________ PMM `kernel_end` reservation ____________________/
//! ```

#[cfg(not(feature = "no-tests"))]
use akuma_selftest::Suite;

use akuma_ryzen_amd64::{MachineDescription, MemRegion};
use crate::phys::{PHYSMAP_LIMIT, phys_to_virt};
use crate::serial;

/// Bytes of RAM handed to the heap, taken off the top of the PMM's range.
///
/// Statically sized because the PMM cannot supply it — see the ordering note
/// above. The PMM's own bitmap (one bit per 4 KiB frame: 512 MiB of RAM costs
/// 16 KiB) is a rounding error here; what actually spends the heap is the
/// scheduler's per-task kernel stacks (two 32 KiB `Vec`s each, `MAX_TASKS`
/// never-recycled slots), a `MAX_PROC_FRAMES`-word `FrameSet` per live process,
/// and the whole-file `Vec` `sys_openat` caches (busybox is ~1.1 MiB, `apk`
/// ~5.4 MiB, and every package `apk add` unpacks passes through one).
///
/// **Raised from 64 MiB to 512 MiB on 2026-09-06.** `apk add tar && apk add
/// tcc` on the HP box drove the 64 MiB heap to exhaustion — `ls` then reported
/// `Out of memory` — because those file-cache `Vec`s are not evicted and a
/// package install reads a dozen of them. The box has 16 GiB; the region below
/// `PHYSMAP_LIMIT` this is carved from has gigabytes free, so this is a safe
/// bump. The real ceiling is [`PHYSMAP_LIMIT`] (4 GiB — `boot.s` maps only the
/// first four): using the full 16 GiB needs more page directories there and is
/// tracked in `docs/archive/AKUMA_SELF_HEALING_PORT.md`.
const HEAP_SIZE: usize = 512 * 1024 * 1024;

const PAGE_SIZE: usize = 4096;

unsafe extern "C" {
    /// End of the linked image including `.bss`, from `linker.ld`.
    static _kernel_end: u8;
}

const fn align_up(v: usize, to: usize) -> usize {
    v.div_ceil(to) * to
}

/// Bring up heap then PMM. Returns false if the machine described no usable RAM.
///
/// Every RAM region the machine reports is managed, across whatever gaps the
/// chipset left between them; only the heap's *placement* still picks one, and
/// it picks the region holding the kernel image when that has room.
pub fn init(machine: &MachineDescription) -> bool {
    init_reserving(machine, 0)
}

/// How a region of RAM ended up being used, for the boot-log accounting.
///
/// This enum exists because the previous version of this function made the same
/// decisions and said nothing about them. On the 16 GiB reference machine it
/// printed `16321 MiB usable` from the banner and `2504 MiB` of free frames four
/// lines later, with no line in between accounting for the difference — so the
/// machine looked like it had lost 85% of its memory to nothing in particular.
/// Every region now says what became of it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fate {
    /// The PMM manages it.
    Pmm,
    /// The PMM manages it, and the heap is carved out of it.
    HeapAndPmm,
    /// Below [`LOW_RAM_FLOOR`] — the BIOS's memory, not ours.
    BelowFloor,
    /// More RAM regions than [`akuma_pmm::MAX_RAM_REGIONS`], so this one cannot
    /// be described to the PMM and must not be handed out. No machine here has
    /// ever reported enough regions to reach this.
    Undescribable,
    /// Past [`PHYSMAP_LIMIT`]: `phys_to_virt` cannot name an address in it.
    Unreachable,
}

impl Fate {
    const fn label(self) -> &'static str {
        match self {
            Self::Pmm => "pmm",
            Self::HeapAndPmm => "heap + pmm",
            Self::BelowFloor => "unused (below the 1 MiB floor)",
            Self::Undescribable => "UNUSED (more regions than the PMM can describe)",
            Self::Unreachable => "UNREACHABLE (past the physmap)",
        }
    }
}

/// A usable region, clipped to the physmap, with the floor its free space
/// actually starts at.
#[derive(Clone, Copy)]
struct Usable {
    base: u64,
    /// `min(region end, PHYSMAP_LIMIT)`.
    end: u64,
    /// `base`, raised past the kernel image and anything the loader placed here.
    floor: u64,
}

impl Usable {
    /// Bytes actually available in this region — what both choices rank on.
    const fn room(self) -> u64 {
        self.end - self.floor
    }
}

/// As [`init`], but keeping the PMM's hands off everything below
/// `reserve_to` as well.
///
/// A multiboot2 boot arrives with the root filesystem already **in RAM**: GRUB
/// loaded it as a module and told us where. Nothing in the memory map says so —
/// the loader reports those frames as ordinary available memory — so without
/// this the PMM would hand out the pages holding the filesystem the kernel is
/// about to mount, and the corruption would appear later and somewhere else.
pub fn init_reserving(machine: &MachineDescription, reserve_to: u64) -> bool {
    let kernel_end = core::ptr::addr_of!(_kernel_end) as u64;

    // EVERY REGION IS MANAGED, NOT ONE, and that is the whole of this function.
    //
    // `akuma_pmm` is a single bitmap over a single base..end range, so for most
    // of this port it was given one region and the rest of the machine's memory
    // was dropped on the floor. That is fine while there *is* one region, which
    // is every aarch64 machine here and QEMU below `-m 4096`. A PC is not one:
    // the chipset leaves a hole below 4 GiB for MMIO and the RAM displaced by it
    // reappears just above 4 GiB, so the map has two runs with a gap between
    // them. Firecracker with 6144 MiB reports ~3 GiB low and 3 GiB high; the
    // trashcan's 16 GiB reports ~3 GiB low and ~13 GiB high.
    //
    // Two earlier answers, both wrong in the same way:
    //
    //   * pick the largest region for both heap and PMM -- loses the other one,
    //     which is 13 GiB of the trashcan's 16;
    //   * (2026-09-12) pick the largest for the PMM and put the heap in the
    //     kernel's region -- loses that region's remainder instead. On the
    //     6 GiB guest the high region won by 5.7 MiB and 2554 MiB went nowhere,
    //     with `free` reporting 3.0G on a box configured for 6.
    //
    // So: one bitmap ACROSS the gap (`init_sparse`), with each run of real
    // memory handed over by `add_ram` and the gap simply never described. The
    // PMM cannot allocate what it was not told about, `total_count` counts only
    // what was described -- it feeds `sysinfo.totalram`, which is what `free`
    // prints -- and `contains` refuses the gap, so the safe physical copies
    // cannot reach the device window the arena now spans.
    //
    // When the machine reports one region both shapes coincide and the result
    // is what this function did before, one `add_ram` call later.
    let mut regions = 0;
    let mut span_base = u64::MAX;
    let mut span_end = 0u64;
    let mut ram_total = 0u64;
    for u in usable_regions(machine, kernel_end, reserve_to).take(akuma_pmm::MAX_RAM_REGIONS) {
        span_base = span_base.min(u.base);
        span_end = span_end.max(u.end);
        ram_total += u.end - u.base;
        regions += 1;
    }

    if regions == 0 {
        serial::puts("  [FATAL] no usable region is reachable through the physmap\n");
        return false;
    }

    // The heap's home: beside the kernel when that fits, the roomiest region
    // otherwise. The fallback is what the 2026-09-06 change to `HEAP_SIZE`
    // needs -- a seven-megabyte UEFI fragment cannot hold 512 MiB, and a boot
    // that refuses on that basis is worse than one that puts the heap next
    // door. Which region it lands in no longer costs anything either way; it
    // decides only where the heap sits, not which memory the machine keeps.
    let mut roomiest: Option<Usable> = None;
    let mut kernel_home: Option<Usable> = None;
    for u in usable_regions(machine, kernel_end, reserve_to).take(regions) {
        if roomiest.is_none_or(|r| u.room() > r.room()) {
            roomiest = Some(u);
        }
        if kernel_end > u.base && kernel_end < u.end {
            kernel_home = Some(u);
        }
    }
    let roomiest = roomiest.expect("regions > 0");
    let heap_home = match kernel_home {
        Some(k) if k.floor.saturating_add(HEAP_SIZE as u64) < k.end => k,
        _ => roomiest,
    };
    let heap_start = heap_home.floor as usize;
    let heap_end = heap_start + HEAP_SIZE;

    // Print the map BEFORE the check that uses it. A "does not fit" message
    // with no sizes in it says only that something is wrong, which is the least
    // useful thing a fatal error can say -- and the accounting below is the
    // whole point of the rest: every reported region, and what became of it.
    let mut seen = 0usize;
    serial::puts("  mem:  RAM the machine reported, and what became of each:\n");
    for r in machine.regions().iter().filter(|r| r.is_ram()) {
        let fate = if r.addr >= PHYSMAP_LIMIT {
            Fate::Unreachable
        } else {
            match usable_of(*r, kernel_end, reserve_to) {
                None => Fate::BelowFloor,
                Some(u) => {
                    let idx = seen;
                    seen += 1;
                    if idx >= regions {
                        Fate::Undescribable
                    } else if u.base == heap_home.base {
                        Fate::HeapAndPmm
                    } else {
                        Fate::Pmm
                    }
                }
            }
        };
        serial::puts("    0x");
        serial::put_hex(r.addr);
        serial::puts(" + ");
        serial::put_dec(r.size / 1024 / 1024);
        serial::puts(" MiB  ");
        serial::puts(fate.label());
        serial::puts("\n");
    }
    // `usable` and `managed` are the same fact counted two ways, and printing
    // both is what makes a repeat of the 2554 MiB loss impossible to miss: they
    // match, or the difference is a number with a region beside it above.
    serial::puts("  mem:  ");
    serial::put_dec(machine.usable_ram() / 1024 / 1024);
    serial::puts(" MiB usable, ");
    serial::put_dec(ram_total / 1024 / 1024);
    serial::puts(" MiB managed, physmap reaches ");
    serial::put_dec(PHYSMAP_LIMIT / 1024 / 1024 / 1024);
    serial::puts(" GiB\n");

    serial::puts("  kernel ends 0x");
    serial::put_hex(kernel_end);
    serial::puts("\n  heap: 0x");
    serial::put_hex(heap_start as u64);
    serial::puts(" + ");
    serial::put_dec((HEAP_SIZE / 1024 / 1024) as u64);
    serial::puts(" MiB ... ");

    if (heap_end as u64) >= heap_home.end {
        serial::puts("\n  [FATAL] heap ends 0x");
        serial::put_hex(heap_end as u64);
        serial::puts(" but the region holding it ends 0x");
        serial::put_hex(heap_home.end);
        serial::puts("\n");
        return false;
    }

    // The allocator hands out pointers, so it must be given the *virtual*
    // address of the heap. Everything else here is physical.
    if let Err(e) = akuma_alloc::init(phys_to_virt(heap_start as u64) as usize, HEAP_SIZE) {
        serial::puts("FAILED: ");
        serial::puts(e);
        serial::puts("\n");
        return false;
    }
    serial::puts("ok\n");

    // The PMM's two registration hooks. Every feature is off and every reclaim
    // hook is a no-op that reclaims nothing: this kernel has no page cache, no
    // retired-process list and no CoW, so a hook that pretended otherwise would
    // be reporting progress it did not make and could spin the OOM path forever.
    akuma_pmm::register_config(akuma_pmm::PmmConfig {
        cow_ref_ledger: false,
        pmm_uaf_quarantine: false,
        pmm_premature_free_check: false,
    });
    akuma_pmm::register_hooks(akuma_pmm::PmmHooks {
        heap_reclaim: || 0,
        // 5b slice 1: the pressure ladder's retired-process rung. Same
        // cooldown-honoring sweep the idle loop and the exit path run; reached
        // from akuma-pmm's allocation pressure path.
        drain_retired: || akuma_exec::process::reclaim::drain_retired(),
        evict_clean_file_pages: |_| 0,
        shrink_page_cache: |_| 0,
    });

    serial::puts("  pmm:  arena 0x");
    serial::put_hex(span_base);
    serial::puts(" + ");
    serial::put_dec((span_end - span_base) / 1024 / 1024);
    serial::puts(" MiB spanning ");
    serial::put_dec(regions as u64);
    serial::puts(" RAM region(s)\n");

    // Nothing is allocatable until `add_ram` describes it, so the order here is
    // load-bearing: describe every region first, then take back what is already
    // spoken for. The reverse would reserve pages that are about to be declared
    // free and hand out the kernel image.
    akuma_pmm::init_sparse(span_base as usize, (span_end - span_base) as usize);
    for u in usable_regions(machine, kernel_end, reserve_to).take(regions) {
        if akuma_pmm::add_ram(u.base as usize, (u.end - u.base) as usize).is_none() {
            // Cannot happen -- the scan above took at most `MAX_RAM_REGIONS` --
            // but a PMM that is handing out an undescribed region is handing out
            // MMIO, so refuse the boot rather than trust the arithmetic.
            serial::puts("  [FATAL] the PMM would not describe region 0x");
            serial::put_hex(u.base);
            serial::puts("\n");
            return false;
        }
        // Whatever already sits at the bottom of this region: the kernel image,
        // and on a multiboot2 boot the root filesystem GRUB left in RAM.
        akuma_pmm::reserve_range(u.base as usize, (u.floor - u.base) as usize);
    }
    // The heap was carved out before the PMM existed and is inside one of the
    // regions just described, so it has to be taken back explicitly.
    akuma_pmm::reserve_range(heap_start, HEAP_SIZE);

    serial::puts("  pmm:  ");
    serial::put_dec((akuma_pmm::total_count() * PAGE_SIZE / 1024 / 1024) as u64);
    serial::puts(" MiB RAM, ");
    serial::put_dec(akuma_pmm::free_count() as u64);
    serial::puts(" free frames (");
    serial::put_dec((akuma_pmm::free_count() * PAGE_SIZE / 1024 / 1024) as u64);
    serial::puts(" MiB)\n");

    true
}

/// The RAM below this is not ours: the interrupt vector table, the BIOS data
/// area and the EBDA live there, and page 0 is a frame whose address reads as a
/// null pointer everywhere it is passed. Every machine here reports it as
/// ordinary available memory; no machine here needs the 639 KiB.
const LOW_RAM_FLOOR: u64 = 0x10_0000;

/// One RAM region as the kernel can actually use it, or `None` if nothing of it
/// is left once the physmap, the low floor and whatever already occupies it are
/// taken off.
fn usable_of(r: MemRegion, kernel_end: u64, reserve_to: u64) -> Option<Usable> {
    let base = r.addr.max(LOW_RAM_FLOOR);
    let end = r.end().min(PHYSMAP_LIMIT);
    if end <= base {
        return None;
    }
    // Anything already occupying part of this region raises the floor.
    let mut floor = base;
    if kernel_end > base && kernel_end < end {
        floor = floor.max(kernel_end);
    }
    if reserve_to > base && reserve_to < end {
        floor = floor.max(reserve_to);
    }
    let floor = align_up(floor as usize, PAGE_SIZE) as u64;
    if floor >= end {
        return None;
    }
    Some(Usable { base, end, floor })
}

/// Every usable RAM region, in the order the machine reported them.
///
/// Deliberately an iterator recomputed at each use rather than an array built
/// once: this runs before the heap exists, so there is nowhere to build one,
/// and the list is walked three times at boot and never again.
fn usable_regions(
    machine: &MachineDescription,
    kernel_end: u64,
    reserve_to: u64,
) -> impl Iterator<Item = Usable> + '_ {
    machine
        .regions()
        .iter()
        .filter(|r| r.is_ram())
        .filter_map(move |r| usable_of(*r, kernel_end, reserve_to))
}

#[cfg(not(feature = "no-tests"))]
/// Exercise both allocators enough to prove they actually work.
///
/// A boot that prints "ok" and never allocates has demonstrated that `init`
/// returned, nothing more. This allocates from the heap, allocates and frees
/// frames, and checks the free count moves in the right direction.
pub fn smoke_test(t: &mut Suite) {
    // Sum of i^2 for i < 4096 = 4095*4096*8191/6. Checked against a value
    // computed here rather than a literal, so the test cannot be "fixed" by
    // pasting in whatever the kernel printed.
    const N: u64 = 4096;
    let want: u64 = (N - 1) * N * (2 * N - 1) / 6;

    let got = {
        let mut v: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
        for i in 0..N {
            v.push(i * i);
        }
        // Read it back so the writes cannot be optimised away.
        v.iter().sum()
    };
    t.check_eq("heap: vec[4096] checksum", got, want);

    let before = akuma_pmm::free_count();
    let mut frames = [0usize; 8];
    let mut got_frames = 0;
    for slot in &mut frames {
        match akuma_pmm::alloc_page() {
            Some(pa) => {
                *slot = pa;
                got_frames += 1;
            }
            None => break,
        }
    }
    let during = akuma_pmm::free_count();
    for &pa in &frames[..got_frames] {
        akuma_pmm::free_page(pa, 0);
    }
    let after = akuma_pmm::free_count();

    t.check_eq("pmm: frames allocated", got_frames as u64, frames.len() as u64);
    t.check_eq("pmm: free count drops", during as u64, (before - frames.len()) as u64);
    t.check_eq("pmm: free count restored", after as u64, before as u64);

    physmap_covers_its_limit(t);
}

/// The physmap reaches as far as [`PHYSMAP_LIMIT`] claims, and does not alias.
///
/// `PHYSMAP_LIMIT` is a Rust constant; the mapping is built by `boot.s`. They
/// are the same fact written in two languages, and nothing links them but
/// `PHYSMAP_PDS` being passed into the second — so this walks the live page
/// tables and asks the question directly, on every boot.
///
/// **The 4 GiB probe is not redundant with the last-page one**, and it is the
/// reason this function exists. The fill loop in `boot.s` builds each entry's
/// physical address in `%eax`, a 32-bit register, because it runs before long
/// mode. Written the obvious way it wraps at the 2048th entry, and the physmap
/// past 4 GiB silently becomes a *second alias of the low 4 GiB* instead of a
/// window onto high memory. Nothing faults, and a kernel walking its own tables
/// through that alias agrees with itself perfectly — the disagreement is only
/// with the CPU's page walker, which reads the real frame. Measured 2026-09-12
/// as a not-present `#PF` on a virtual address `paging::translate` reported, in
/// the same breath, as correctly mapped. A `translate` of `4 GiB` returning `0`
/// is that bug, stated as a number.
#[cfg(not(feature = "no-tests"))]
fn physmap_covers_its_limit(t: &mut Suite) {
    // Probed at the last page rather than at the limit itself: the limit is
    // one past the end, and `phys_to_virt` asserts on it.
    let last = PHYSMAP_LIMIT - PAGE_SIZE as u64;
    t.check_eq(
        "physmap: reaches PHYSMAP_LIMIT",
        crate::paging::translate(phys_to_virt(last) as usize).unwrap_or(u64::MAX),
        last,
    );
    // The first page the old 32-bit fill loop got wrong.
    const FOUR_GIB: u64 = 4 << 30;
    t.check_eq(
        "physmap: 4 GiB does not alias physical 0",
        crate::paging::translate(phys_to_virt(FOUR_GIB) as usize).unwrap_or(u64::MAX),
        FOUR_GIB,
    );
}

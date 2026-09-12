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

use akuma_ryzen_amd64::MachineDescription;
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
/// The region is chosen by **containment** — `region_containing`, in
/// `akuma-ryzen-amd64` and host-tested there. The largest usable region is very
/// nearly always the right one, but "the region holding the kernel" is right by
/// construction: picking any other would hand the PMM frames while the kernel
/// image sits somewhere it has never heard of.
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
    /// The heap is carved out of it, and the PMM manages the rest.
    HeapAndPmm,
    /// The heap is carved out of it; the PMM is somewhere else.
    Heap,
    /// Reachable, but the PMM manages exactly one region and this is not it.
    Unused,
    /// Past [`PHYSMAP_LIMIT`]: `phys_to_virt` cannot name an address in it.
    Unreachable,
}

impl Fate {
    const fn label(self) -> &'static str {
        match self {
            Self::Pmm => "pmm",
            Self::HeapAndPmm => "heap + pmm",
            Self::Heap => "heap",
            Self::Unused => "unused (the PMM manages one region)",
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

    // TWO REGIONS ARE CHOSEN HERE, NOT ONE, and separating them is the fix.
    //
    // Until 2026-09-12 this picked a single region for both the heap and the
    // PMM, by "most room after everything already in it". That was written for
    // UEFI, and it was right about the problem it was written for: the firmware
    // map is carved up by how the firmware used memory, and the region
    // *containing the kernel* on the reference machine runs 0x100000..0x800000
    // -- seven megabytes on a box with sixteen gigabytes -- so a rule of
    // "containment" put a 64 MiB heap somewhere it did not fit.
    //
    // One region for both is fine while there is only one big one. On a PC
    // there are two, because a PC displaces the RAM behind the MMIO hole to
    // just above 4 GiB: a 16 GiB machine reports ~3 GiB low and ~13 GiB high.
    // Ranking those and taking one means the *other* is dropped entirely, and
    // whichever way the rank goes the machine loses gigabytes. Before the
    // physmap reached past 4 GiB the high region was not even a candidate and
    // the answer was always the low one; raising the limit alone would simply
    // have moved the loss to the other side.
    //
    // So:
    //   * the PMM gets the largest reachable region, because it is the one that
    //     has to hold every process;
    //   * the heap is carved out of the region holding the kernel image, when
    //     that region has room -- which keeps the heap, the kernel and a
    //     loader-placed module together in low memory, and leaves the PMM's
    //     region whole.
    //
    // When the machine reports one region (every VMM guest, and QEMU below
    // `-m 4096`) both rules select it and the result is byte-identical to what
    // this function did before.
    let mut pmm: Option<Usable> = None;
    let mut kernel_home: Option<Usable> = None;
    for r in machine.regions().iter().filter(|r| r.is_ram()) {
        let base = r.addr;
        let end = r.end().min(PHYSMAP_LIMIT);
        if end <= base {
            continue; // entirely past the physmap
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
            continue;
        }
        let u = Usable { base, end, floor };
        if pmm.is_none_or(|p| u.room() > p.room()) {
            pmm = Some(u);
        }
        if kernel_end > base && kernel_end < end {
            kernel_home = Some(u);
        }
    }

    let Some(pmm) = pmm else {
        serial::puts("  [FATAL] no usable region is reachable through the physmap\n");
        return false;
    };

    // The heap's home: beside the kernel when that fits, the PMM's region
    // otherwise. The fallback is what the 2026-09-06 change to `HEAP_SIZE`
    // needs -- a seven-megabyte UEFI fragment cannot hold 512 MiB, and a boot
    // that refuses on that basis is worse than one that shares.
    let heap_home = match kernel_home {
        Some(k) if k.floor.saturating_add(HEAP_SIZE as u64) < k.end => k,
        _ => pmm,
    };
    let shared = heap_home.base == pmm.base;
    let heap_start = heap_home.floor as usize;
    let heap_end = heap_start + HEAP_SIZE;

    // Print the map BEFORE the check that uses it. A "does not fit" message
    // with no sizes in it says only that something is wrong, which is the least
    // useful thing a fatal error can say -- and the accounting below is the
    // whole point of the rest: every reported region, and what became of it.
    let managed = pmm.end - pmm.base;
    serial::puts("  mem:  RAM the machine reported, and what became of each:\n");
    for r in machine.regions().iter().filter(|r| r.is_ram()) {
        let fate = if r.addr >= PHYSMAP_LIMIT {
            Fate::Unreachable
        } else if r.addr == pmm.base {
            if shared { Fate::HeapAndPmm } else { Fate::Pmm }
        } else if r.addr == heap_home.base {
            Fate::Heap
        } else {
            Fate::Unused
        };
        serial::puts("    0x");
        serial::put_hex(r.addr);
        serial::puts(" + ");
        serial::put_dec(r.size / 1024 / 1024);
        serial::puts(" MiB  ");
        serial::puts(fate.label());
        serial::puts("\n");
    }
    serial::puts("  mem:  ");
    serial::put_dec(machine.usable_ram() / 1024 / 1024);
    serial::puts(" MiB usable, ");
    serial::put_dec(managed / 1024 / 1024);
    serial::puts(" MiB in the PMM's region, physmap reaches ");
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

    // What the PMM must keep its hands off, and it is not always the same thing.
    //
    // When the heap shares the PMM's region it is `heap_end`: the heap was
    // carved out before the PMM existed, so it has to be inside the reservation
    // or the PMM hands out frames the allocator is already using. When the heap
    // lives elsewhere there is nothing of the kernel's in this region, and the
    // reservation is only whatever the loader placed here -- which `floor`
    // already accounts for, and which is `base` when there is none.
    let reserved_to = if shared { heap_end } else { pmm.floor as usize };
    let ram_size = (pmm.end - pmm.base) as usize;
    serial::puts("  pmm:  init(base=0x");
    serial::put_hex(pmm.base);
    serial::puts(", size=");
    serial::put_dec((ram_size / 1024 / 1024) as u64);
    serial::puts(" MiB, reserved_to=0x");
    serial::put_hex(reserved_to as u64);
    serial::puts(")\n");

    akuma_pmm::init(pmm.base as usize, ram_size, reserved_to);

    serial::puts("  pmm:  ");
    serial::put_dec(akuma_pmm::free_count() as u64);
    serial::puts(" free frames (");
    serial::put_dec((akuma_pmm::free_count() * PAGE_SIZE / 1024 / 1024) as u64);
    serial::puts(" MiB)\n");

    true
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

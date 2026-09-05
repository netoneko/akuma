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

/// As [`init`], but keeping the PMM's hands off everything below
/// `reserve_to` as well.
///
/// A multiboot2 boot arrives with the root filesystem already **in RAM**: GRUB
/// loaded it as a module and told us where. Nothing in the memory map says so —
/// the loader reports those frames as ordinary available memory — so without
/// this the PMM would hand out the pages holding the filesystem the kernel is
/// about to mount, and the corruption would appear later and somewhere else.
pub fn init_reserving(machine: &MachineDescription, reserve_to: u64) -> bool {
    let kernel_end = core::ptr::addr_of!(_kernel_end) as usize;

    // WHICH REGION THE HEAP GOES IN, and this is not the obvious choice.
    //
    // It used to be "the region containing the kernel image", which is right on
    // a VMM: those report two or three big regions and the kernel is in the
    // large one. **UEFI is not like that.** Its map is carved up by how the
    // firmware itself used memory, and on the reference machine the region
    // containing the kernel runs `0x100000..0x800000` -- seven megabytes, on a
    // box with sixteen gigabytes -- with the rest of RAM in other regions
    // entirely. A 64 MiB heap does not fit in seven, and the boot failed there.
    //
    // So: take whichever usable region has the most room *after* everything
    // already sitting in it. Containment stops being needed once the PMM is
    // given a single region, because anything outside that region is never
    // handed out at all -- which is exactly what protects a kernel image, or a
    // loader-placed module, that lives somewhere else.
    let mut choice: Option<(u64, u64, usize)> = None; // (base, end, floor)
    for r in machine.regions().iter().filter(|r| r.is_ram()) {
        let base = r.addr;
        let end = r.end().min(PHYSMAP_LIMIT);
        if end <= base {
            continue;
        }
        // Anything already occupying part of this region raises the floor the
        // heap may start at.
        let mut floor = base;
        if (kernel_end as u64) > base && (kernel_end as u64) < end {
            floor = floor.max(kernel_end as u64);
        }
        if reserve_to > base && reserve_to < end {
            floor = floor.max(reserve_to);
        }
        let floor = align_up(floor as usize, PAGE_SIZE);
        if (floor as u64).saturating_add(HEAP_SIZE as u64) >= end {
            continue;
        }
        let room = end - floor as u64;
        if choice.is_none_or(|(_, prev_end, prev_floor)| room > prev_end - prev_floor as u64) {
            choice = Some((base, end, floor));
        }
    }

    let Some((ram_base, ram_end, heap_start)) = choice else {
        serial::puts("  [FATAL] no usable region has room for the heap\n");
        return false;
    };

    // `heap_start` came out of the region choice above, already raised past the
    // kernel image and past anything the loader placed in this region: a boot
    // loader is free to drop its modules wherever it finds space, and "just
    // after the kernel" is a favourite. Overlapping them would hand the
    // allocator the filesystem the kernel is about to mount.
    let heap_end = heap_start + HEAP_SIZE;

    // Print the numbers BEFORE the check that uses them. A "does not fit"
    // message with no sizes in it says only that something is wrong, which is
    // the least useful thing a fatal error can say.
    serial::puts("  ram:  0x");
    serial::put_hex(ram_base);
    serial::puts(" .. 0x");
    serial::put_hex(ram_end);
    serial::puts("\n  kernel ends 0x");
    serial::put_hex(kernel_end as u64);
    serial::puts("\n  heap: 0x");
    serial::put_hex(heap_start as u64);
    serial::puts(" + ");
    serial::put_dec((HEAP_SIZE / 1024 / 1024) as u64);
    serial::puts(" MiB ... ");

    if (heap_end as u64) >= ram_end {
        serial::puts("\n  [FATAL] heap ends 0x");
        serial::put_hex(heap_end as u64);
        serial::puts(" but the region holding the kernel ends 0x");
        serial::put_hex(ram_end);
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
        drain_retired: || 0,
        evict_clean_file_pages: |_| 0,
        shrink_page_cache: |_| 0,
    });

    // `heap_end`, not `kernel_end`: the heap was carved out of this same region
    // before the PMM existed, so it must be inside the reservation or the PMM
    // will hand out frames the allocator is already using.
    let ram_size = (ram_end - ram_base) as usize;
    serial::puts("  pmm:  init(base=0x");
    serial::put_hex(ram_base);
    serial::puts(", size=");
    serial::put_dec((ram_size / 1024 / 1024) as u64);
    serial::puts(" MiB, reserved_to=0x");
    serial::put_hex(heap_end as u64);
    serial::puts(")\n");

    akuma_pmm::init(ram_base as usize, ram_size, heap_end);

    serial::puts("  pmm:  ");
    serial::put_dec(akuma_pmm::free_count() as u64);
    serial::puts(" free frames (");
    serial::put_dec((akuma_pmm::free_count() * PAGE_SIZE / 1024 / 1024) as u64);
    serial::puts(" MiB)\n");

    true
}

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
}

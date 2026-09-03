//! Stage A: bring up the kernel heap and the physical frame allocator on x86_64.
//!
//! Both `akuma-alloc` and `akuma-pmm` are architecture-neutral and built for
//! `x86_64-unknown-none` before this target existed. This module is the test of
//! that: if the claim in `proposals/REDUCING_PLATFORM_DEPENDENCY.md` is right,
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

use crate::hvm::StartInfo;
use crate::serial;

/// Bytes of RAM handed to the heap, taken off the top of the PMM's range.
///
/// Statically sized because the PMM cannot supply it — see the ordering note
/// above. 16 MiB is chosen to be comfortably more than the PMM's own bitmap
/// needs (one bit per 4 KiB frame: 512 MiB of RAM costs 16 KiB of bitmap) while
/// staying negligible against any plausible guest.
const HEAP_SIZE: usize = 16 * 1024 * 1024;

/// `boot.s` identity-maps exactly the first 1 GiB. Nothing above it is
/// addressable yet, so RAM beyond this is not ours to hand out however much of
/// it the VMM reports.
const IDENTITY_MAP_LIMIT: u64 = 1 << 30;

const PAGE_SIZE: usize = 4096;

unsafe extern "C" {
    /// End of the linked image including `.bss`, from `linker.ld`.
    static _kernel_end: u8;
}

const fn align_up(v: usize, to: usize) -> usize {
    v.div_ceil(to) * to
}

/// Pick the RAM region the kernel was loaded into.
///
/// Chosen by *containment*, not by size. The largest usable region is very
/// nearly always the right one, but "the region holding the kernel" is the one
/// that is right by construction — picking any other would hand the PMM frames
/// while the kernel image sits somewhere it has never heard of.
fn pick_region(info: &StartInfo, kernel_end: usize) -> Option<(u64, u64)> {
    for i in 0..info.memmap_entries {
        // SAFETY: index and fields are bounds-checked inside.
        let Some(r) = (unsafe { info.memmap_entry(i) }) else {
            continue;
        };
        if !r.is_ram() {
            continue;
        }
        let end = r.addr.checked_add(r.size)?;
        if r.addr <= kernel_end as u64 && (kernel_end as u64) < end {
            // Clamp to what boot.s actually mapped.
            return Some((r.addr, end.min(IDENTITY_MAP_LIMIT)));
        }
    }
    None
}

/// Bring up heap then PMM. Returns false if the machine described no usable RAM.
pub fn init(info: &StartInfo) -> bool {
    let kernel_end = core::ptr::addr_of!(_kernel_end) as usize;

    let Some((ram_base, ram_end)) = pick_region(info, kernel_end) else {
        serial::puts("  [FATAL] no RAM region contains the kernel image\n");
        return false;
    };

    let heap_start = align_up(kernel_end, PAGE_SIZE);
    let heap_end = heap_start + HEAP_SIZE;

    if (heap_end as u64) >= ram_end {
        serial::puts("  [FATAL] heap does not fit in the region holding the kernel\n");
        return false;
    }

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

    if let Err(e) = akuma_alloc::init(heap_start, HEAP_SIZE) {
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
/// frames, and checks the free count moves in the right direction — the smallest
/// thing that fails loudly if the wiring is wrong.
pub fn smoke_test() {
    serial::puts("  test: heap ");
    {
        let mut v: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
        for i in 0..4096u64 {
            v.push(i * i);
        }
        // Read it back so the writes cannot be optimised away.
        let sum: u64 = v.iter().sum();
        serial::puts("vec[4096] sum=");
        serial::put_dec(sum);
    }

    let before = akuma_pmm::free_count();
    let mut frames = [0usize; 8];
    let mut got = 0;
    for slot in &mut frames {
        match akuma_pmm::alloc_page() {
            Some(pa) => {
                *slot = pa;
                got += 1;
            }
            None => break,
        }
    }
    let during = akuma_pmm::free_count();
    for &pa in &frames[..got] {
        akuma_pmm::free_page(pa, 0);
    }
    let after = akuma_pmm::free_count();

    serial::puts("\n  test: pmm alloc ");
    serial::put_dec(got as u64);
    serial::puts(" frames, free ");
    serial::put_dec(before as u64);
    serial::puts(" -> ");
    serial::put_dec(during as u64);
    serial::puts(" -> ");
    serial::put_dec(after as u64);

    if got == frames.len() && during == before - got && after == before {
        serial::puts("   [OK]\n");
    } else {
        serial::puts("   [FAIL]\n");
    }
}

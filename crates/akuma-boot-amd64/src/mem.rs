//! Stage A: bring up the kernel heap and the physical frame allocator on x86_64
//! — the machine half of `amd64/src/mem.rs`, extracted so a composed kernel
//! pulls one `mem::init` rather than inheriting the module.
//!
//! # Ordering, which is not obvious and not optional
//!
//! **The heap must come up before the PMM.** `akuma_pmm`'s `init` allocates its
//! own free-page bitmap with `alloc::vec![0u64; n]`, so a PMM initialised
//! before there is a heap faults inside the allocator. The dependency runs the
//! opposite way to the intuition that a frame allocator is the more primitive
//! thing.
//!
//! # What each kernel supplies
//!
//! * the **console**: every diagnostic here goes through
//!   `akuma_primitives`' print hook (`safe_print!`), so the caller's console —
//!   ring and framebuffer mirror, or a bare COM port — is the one that prints.
//!   Register the hook before calling [`init_reserving`]; an unregistered hook
//!   silently drops these lines.
//! * the **PMM hooks** (`PmmHooks`): the full kernel registers
//!   `akuma-exec`'s retired-process reaper; a kernel with no processes passes
//!   no-ops.
//! * the **forensics flag**: the full kernel's `pmm-forensics` feature.

use akuma_ryzen_amd64::{MachineDescription, MemRegion};
use akuma_primitives::safe_print;

use crate::phys::{PHYSMAP_LIMIT, phys_to_virt};

/// Bytes of RAM handed to the heap, taken off the top of the PMM's range.
///
/// Statically sized because the PMM cannot supply it — see the ordering note
/// above. The PMM's own bitmap (one bit per 4 KiB frame: 512 MiB of RAM costs
/// 16 KiB) is a rounding error here.
///
/// **Raised from 64 MiB to 512 MiB on 2026-09-06.** `apk add tar && apk add
/// tcc` on the HP box drove the 64 MiB heap to exhaustion — `ls` then reported
/// `Out of memory` — because whole-file cache `Vec`s are not evicted and a
/// package install reads a dozen of them (the history, and the consumers that
/// set this floor, are documented on `akuma-amd64`'s own copy of this constant).
pub const HEAP_SIZE: usize = 512 * 1024 * 1024;

/// The heap on a machine with enough RAM to afford a bigger one: **1 GiB when
/// the machine reports ≥ 8 GiB usable**, [`HEAP_SIZE`] otherwise. A *request*,
/// never a demand — [`init_reserving`] falls back to [`HEAP_SIZE`] when no
/// reachable region can hold it, because a machine that boots today must not
/// stop booting for want of a bigger heap.
pub const HEAP_SIZE_LARGE: usize = 1024 * 1024 * 1024;

/// RAM at or above which [`HEAP_SIZE_LARGE`] is requested.
pub const LARGE_HEAP_RAM_THRESHOLD: u64 = 8 * 1024 * 1024 * 1024;

/// The heap size to *request* for a machine reporting `usable_ram` bytes.
///
/// Pure, so the policy can be read and tested without booting.
#[must_use]
pub const fn heap_size_for(usable_ram: u64) -> usize {
    if usable_ram >= LARGE_HEAP_RAM_THRESHOLD {
        HEAP_SIZE_LARGE
    } else {
        HEAP_SIZE
    }
}

/// The heap size this boot actually got, for code that runs after
/// [`init_reserving`].
///
/// `HEAP_SIZE` is only the floor — the one late consumer (the full kernel's
/// fs block-cache quarter-rule) would silently under-size the cache by half on
/// a big machine if it kept using the constant.
static HEAP_BYTES: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(HEAP_SIZE);

/// Bytes of heap this boot brought up.
#[must_use]
pub fn heap_size() -> usize {
    HEAP_BYTES.load(core::sync::atomic::Ordering::Relaxed)
}

const PAGE_SIZE: usize = 4096;

unsafe extern "C" {
    /// End of the linked image including `.bss`, from the linker script.
    static _kernel_end: u8;
}

const fn align_up(v: usize, to: usize) -> usize {
    v.div_ceil(to) * to
}

/// Bring up heap then PMM. Returns false if the machine described no usable RAM.
pub fn init(machine: &MachineDescription, forensics: bool, hooks: akuma_pmm::PmmHooks) -> bool {
    init_reserving(machine, &[], forensics, hooks)
}

/// How a region of RAM ended up being used, for the boot-log accounting.
///
/// This enum exists because the previous version of this function made the same
/// decisions and said nothing about them. On the 16 GiB reference machine it
/// printed `16321 MiB usable` from the banner and `2504 MiB` of free frames
/// four lines later, with no line in between accounting for the difference — so
/// the machine looked like it had lost 85% of its memory to nothing in
/// particular. Every region now says what became of it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fate {
    /// The PMM manages it.
    Pmm,
    /// The PMM manages it, and the heap is carved out of it.
    HeapAndPmm,
    /// Below [`LOW_RAM_FLOOR`] — the BIOS's memory, not ours.
    BelowFloor,
    /// Wholly inside the kernel image or a loader module. UEFI splits the
    /// image's range into several regions; see [`usable_of`].
    Occupied,
    /// More RAM regions than [`akuma_pmm::MAX_RAM_REGIONS`], so this one cannot
    /// be described to the PMM and must not be handed out.
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
            Self::Occupied => "occupied (kernel image / loader module)",
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

/// As [`init`], but keeping the PMM's hands off every `[start, end)` in
/// `reserved` as well — on a multiboot2 boot, the modules and the information
/// block, both of which the kernel goes on reading after this returns.
///
/// `forensics` turns on the PMM's quarantine/premature-free/CoW-ledger
/// instruments (see `akuma-amd64`'s `pmm-forensics` feature for the costs);
/// `hooks` are registered verbatim into `akuma_pmm`.
pub fn init_reserving(
    machine: &MachineDescription,
    reserved: &[(u64, u64)],
    forensics: bool,
    hooks: akuma_pmm::PmmHooks,
) -> bool {
    let kernel_end = core::ptr::addr_of!(_kernel_end) as u64;
    // From the 1 MiB floor, not from `_kernel_start` (2 MiB): the layout above
    // has always kept the PMM off everything below `kernel_end`, and the AP
    // trampoline and loader structures have lived in that MiB on some boots.
    let kernel = (LOW_RAM_FLOOR, kernel_end);
    // Fixed capacity: this runs before the heap exists.
    let mut taken = [(0u64, 0u64); 4];
    taken[0] = kernel;
    for (slot, r) in taken[1..].iter_mut().zip(reserved) {
        *slot = *r;
    }
    if reserved.len() > taken.len() - 1 {
        safe_print!(128, "  [FATAL] more reserved spans than mem::init_reserving holds\n");
        return false;
    }

    // EVERY REGION IS MANAGED, NOT ONE, and that is the whole of this function.
    //
    // `akuma_pmm` is a single bitmap over a single base..end range, so for most
    // of the amd64 port it was given one region and the rest of the machine's
    // memory was dropped on the floor. A PC is not one region: the chipset
    // leaves a hole below 4 GiB for MMIO and the RAM displaced by it reappears
    // just above 4 GiB. So: one bitmap ACROSS the gap (`init_sparse`), with each
    // run of real memory handed over by `add_ram` and the gap simply never
    // described.
    let mut regions = 0;
    let mut span_base = u64::MAX;
    let mut span_end = 0u64;
    let mut ram_total = 0u64;
    for u in usable_regions(machine, &taken).take(akuma_pmm::MAX_RAM_REGIONS) {
        span_base = span_base.min(u.base);
        span_end = span_end.max(u.end);
        ram_total += u.end - u.base;
        regions += 1;
    }

    if regions == 0 {
        safe_print!(128, "  [FATAL] no usable region is reachable through the physmap\n");
        return false;
    }

    // The heap's home: beside the kernel when that fits, the roomiest region
    // otherwise.
    let mut roomiest: Option<Usable> = None;
    let mut kernel_home: Option<Usable> = None;
    for u in usable_regions(machine, &taken).take(regions) {
        if roomiest.is_none_or(|r| u.room() > r.room()) {
            roomiest = Some(u);
        }
        if kernel_end > u.base && kernel_end < u.end {
            kernel_home = Some(u);
        }
    }
    let roomiest = roomiest.expect("regions > 0");
    // Ask for the large heap on a large machine — but a *request*, never a
    // demand. See `HEAP_SIZE_LARGE`.
    let pick = |want: usize| -> Option<(Usable, usize, u64)> {
        let at = |u: Usable| heap_start_in(u, want as u64, &taken).map(|s| (u, want, s));
        kernel_home.and_then(at).or_else(|| at(roomiest))
    };
    let (heap_home, heap_bytes, heap_at) = pick(heap_size_for(machine.usable_ram()))
        .or_else(|| pick(HEAP_SIZE))
        // Nothing fits at either size. Keep the historical choice so the
        // explicit diagnostic below reports against the same region it always
        // did.
        .unwrap_or((roomiest, HEAP_SIZE, roomiest.floor));
    HEAP_BYTES.store(heap_bytes, core::sync::atomic::Ordering::Relaxed);
    let heap_start = heap_at as usize;
    let heap_end = heap_start + heap_bytes;

    // Print the map BEFORE the check that uses it. A "does not fit" message
    // with no sizes in it says only that something is wrong, which is the least
    // useful thing a fatal error can say — and the accounting below is the
    // whole point of the rest: every reported region, and what became of it.
    let mut seen = 0usize;
    safe_print!(96, "  mem:  RAM the machine reported, and what became of each:\n");
    for r in machine.regions().iter().filter(|r| r.is_ram()) {
        let fate = if r.addr >= PHYSMAP_LIMIT {
            Fate::Unreachable
        } else {
            match usable_of(*r, &taken) {
                None if r.end() <= LOW_RAM_FLOOR => Fate::BelowFloor,
                None => Fate::Occupied,
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
        safe_print!(
            160,
            "    0x{:016x} + {} MiB  {}\n",
            r.addr,
            r.size / 1024 / 1024,
            fate.label()
        );
    }
    // `usable` and `managed` are the same fact counted two ways, and printing
    // both is what makes a repeat of a silent partial-PMM loss impossible to
    // miss: they match, or the difference is a number with a region beside it
    // above.
    safe_print!(
        160,
        "  mem:  {} MiB usable, {} MiB managed, physmap reaches {} GiB\n",
        machine.usable_ram() / 1024 / 1024,
        ram_total / 1024 / 1024,
        PHYSMAP_LIMIT / 1024 / 1024 / 1024
    );

    safe_print!(
        160,
        "  kernel ends 0x{:016x}\n  heap: 0x{:016x} + {} MiB ... ",
        kernel_end,
        heap_start as u64,
        heap_bytes / 1024 / 1024
    );

    if heap_end as u64 >= heap_home.end {
        safe_print!(
            192,
            "\n  [FATAL] heap ends 0x{:016x} but the region holding it ends 0x{:016x}\n",
            heap_end as u64,
            heap_home.end
        );
        return false;
    }

    // The allocator hands out pointers, so it must be given the *virtual*
    // address of the heap. Everything else here is physical.
    if let Err(e) = akuma_alloc::init(phys_to_virt(heap_start as u64) as usize, heap_bytes) {
        safe_print!(96, "FAILED: {}\n", e);
        return false;
    }
    safe_print!(8, "ok\n");

    // The PMM's config and hooks. `forensics` is the caller's choice about
    // cost, not a statement about capability; the hooks are the caller's own
    // reapers (the full kernel's `drain_retired` reaches into `akuma-exec`).
    akuma_pmm::register_config(akuma_pmm::PmmConfig {
        cow_ref_ledger: forensics,
        pmm_uaf_quarantine: forensics,
        pmm_premature_free_check: forensics,
    });
    if forensics {
        safe_print!(
            96,
            "  pmm:  forensics ON (quarantine + premature-free + CoW ledger)\n"
        );
    }
    akuma_pmm::register_hooks(hooks);

    safe_print!(
        160,
        "  pmm:  arena 0x{:016x} + {} MiB spanning {} RAM region(s)\n",
        span_base,
        (span_end - span_base) / 1024 / 1024,
        regions
    );

    // Nothing is allocatable until `add_ram` describes it, so the order here is
    // load-bearing: describe every region first, then take back what is already
    // spoken for. The reverse would reserve pages that are about to be declared
    // free and hand out the kernel image.
    akuma_pmm::init_sparse(span_base as usize, (span_end - span_base) as usize);
    for u in usable_regions(machine, &taken).take(regions) {
        if akuma_pmm::add_ram(u.base as usize, (u.end - u.base) as usize).is_none() {
            // Cannot happen -- the scan above took at most `MAX_RAM_REGIONS` --
            // but a PMM that is handing out an undescribed region is handing
            // out MMIO, so refuse the boot rather than trust the arithmetic.
            safe_print!(
                128,
                "  [FATAL] the PMM would not describe region 0x{:016x}\n",
                u.base
            );
            return false;
        }
        // Whatever already sits at the bottom of this region: the kernel image,
        // and on a multiboot2 boot the root filesystem GRUB left in RAM.
        akuma_pmm::reserve_range(u.base as usize, (u.floor - u.base) as usize);
        // And whatever sits inside it: the holes `usable_of` left in place.
        for &(a, b) in &taken {
            let lo = a.max(u.floor) & !(PAGE_SIZE as u64 - 1);
            let hi = align_up(b.min(u.end) as usize, PAGE_SIZE) as u64;
            if b > a && hi > lo {
                akuma_pmm::reserve_range(lo as usize, (hi - lo) as usize);
            }
        }
    }
    // The heap was carved out before the PMM existed and is inside one of the
    // regions just described, so it has to be taken back explicitly.
    akuma_pmm::reserve_range(heap_start, heap_bytes);

    safe_print!(
        160,
        "  pmm:  {} MiB RAM, {} free frames ({} MiB)\n",
        (akuma_pmm::total_count() * PAGE_SIZE / 1024 / 1024) as u64,
        akuma_pmm::free_count() as u64,
        (akuma_pmm::free_count() * PAGE_SIZE / 1024 / 1024) as u64
    );

    true
}

/// The RAM below this is not ours: the interrupt vector table, the BIOS data
/// area and the EBDA live there, and page 0 is a frame whose address reads as a
/// null pointer everywhere it is passed.
const LOW_RAM_FLOOR: u64 = 0x10_0000;

/// One RAM region as the kernel can actually use it, or `None` if nothing of it
/// is left once the physmap, the low floor and whatever already occupies it are
/// taken off.
///
/// `taken` is every `[start, end)` already occupied — the kernel image and the
/// loader's modules — and **each is carved out of every region it overlaps**.
/// Raising only the containing region's floor is correct only while the image
/// sits inside one region; UEFI firmware splits low memory by its own past use,
/// and the kernel image can span two regions (the measured incident: the PMM
/// handed out the kernel's own `.text`/`.data` as free frames — a `#PF` inside
/// the allocator, a silent triple fault with no console). A span covering the
/// region's bottom raises `floor` past it; one covering the top clips `end`
/// below it. A span strictly **inside** `[floor, end)` is left in the region
/// and reserved as a hole by [`init_reserving`]; the heap is placed around it.
fn usable_of(r: MemRegion, taken: &[(u64, u64)]) -> Option<Usable> {
    let base = r.addr.max(LOW_RAM_FLOOR);
    let mut end = r.end().min(PHYSMAP_LIMIT);
    if end <= base {
        return None;
    }
    let mut floor = base;
    for &(start, stop) in taken {
        if stop <= start || stop <= floor || start >= end {
            continue; // empty, or no overlap with what is left
        }
        if start <= floor {
            floor = stop;
        } else if stop >= end {
            end = start;
        }
    }
    let floor = align_up(floor as usize, PAGE_SIZE) as u64;
    if floor >= end {
        return None;
    }
    Some(Usable { base, end, floor })
}

/// Where in `u` a heap of `want` bytes can start without overlapping any
/// `taken` span left inside the region, lowest first; `None` if it fits
/// nowhere. Same strict `<` against `end` the single-span check always used.
fn heap_start_in(u: Usable, want: u64, taken: &[(u64, u64)]) -> Option<u64> {
    let mut s = u.floor;
    'again: loop {
        let e = s.checked_add(want)?;
        if e >= u.end {
            return None;
        }
        for &(a, b) in taken {
            if b > a && a < e && b > s {
                s = align_up(b as usize, PAGE_SIZE) as u64;
                continue 'again;
            }
        }
        return Some(s);
    }
}

/// Every usable RAM region, in the order the machine reported them.
///
/// Deliberately an iterator recomputed at each use rather than an array built
/// once: this runs before the heap exists, so there is nowhere to build one,
/// and the list is walked three times at boot and never again.
fn usable_regions<'a>(
    machine: &'a MachineDescription,
    taken: &'a [(u64, u64)],
) -> impl Iterator<Item = Usable> + 'a {
    machine
        .regions()
        .iter()
        .filter(|r| r.is_ram())
        .filter_map(move |r| usable_of(*r, taken))
}

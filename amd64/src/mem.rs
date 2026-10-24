//! Bring up the kernel heap and the physical frame allocator on x86_64.
//!
//! The policy — every region managed across the MMIO gap, the heap placed
//! beside the kernel, the boot-log accounting of what became of each region —
//! lives in `akuma-boot-amd64::mem` since the Taoxichuan stage-1 extraction
//! (2026-10-24); this module is this kernel's thin half: the `akuma-exec`
//! reaper hook the crate takes as a parameter, the `pmm-forensics` feature
//! flag, and the boot self-test suite.
//!
//! Ordering, which the crate owns and this kernel relies on: **heap before
//! PMM** (`akuma_pmm`'s own init allocates its bitmap). Diagnostics go through
//! `akuma_primitives`' print hook, which `kmain` points at this kernel's
//! console before the first call here.

#[cfg(not(feature = "no-tests"))]
use akuma_selftest::Suite;

use akuma_ryzen_amd64::MachineDescription;

pub use akuma_boot_amd64::mem::{HEAP_SIZE, heap_size, heap_size_for};

// The self-test's physmap walk, under the same `no-tests` gate as the suite.
#[cfg(not(feature = "no-tests"))]
use crate::phys::phys_to_virt;

/// Bring up heap then PMM. Returns false if the machine described no usable RAM.
pub fn init(machine: &MachineDescription) -> bool {
    init_reserving(machine, &[])
}

/// As [`init`], but keeping the PMM's hands off every `[start, end)` in
/// `reserved` as well — on a multiboot2 boot, the modules and the information
/// block, both of which the kernel goes on reading after this returns.
pub fn init_reserving(machine: &MachineDescription, reserved: &[(u64, u64)]) -> bool {
    akuma_boot_amd64::mem::init_reserving(
        machine,
        reserved,
        cfg!(feature = "pmm-forensics"),
        akuma_pmm::PmmHooks {
            heap_reclaim: || 0,
            // 5b slice 1: the pressure ladder's retired-process rung. Same
            // cooldown-honoring sweep the idle loop and the exit path run;
            // reached from akuma-pmm's allocation pressure path.
            drain_retired: || akuma_exec::process::reclaim::drain_retired(),
            evict_clean_file_pages: |_| 0,
            shrink_page_cache: |_| 0,
        },
    )
}

#[cfg(not(feature = "no-tests"))]
/// Exercise both allocators enough to prove they actually work.
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

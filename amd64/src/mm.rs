//! Anonymous `mmap` for ring 3.
//!
//! The one memory syscall a userspace allocator needs. `libakuma`'s global
//! allocator is mmap-based — it had a `brk` arm once and that was removed — so
//! without this no program that allocates can run, which is every program more
//! complex than the Stage L probe.
//!
//! # Eager, not demand-paged
//!
//! Every page is allocated and mapped at `mmap` time. The `#PF` handler can
//! service a not-present fault (Stage C), so demand paging is reachable, but it
//! needs a per-address-space region table to know *which* faults to service —
//! and inventing one here would be building the thing `akuma-mmap` already is.
//! Eager mapping is honest and bounded: a program that asks for 64 MiB gets 64
//! MiB of frames or an error, rather than a mapping that fails later at a
//! random instruction.
//!
//! # What is refused
//!
//! File-backed mappings, and `MAP_FIXED`. Both need something this target does
//! not have — a page cache for the first, a region table for the second.
//! Refusing is the point: a file-backed `mmap` that quietly returned zeroed
//! anonymous memory would look like a working call and hand the caller a file
//! full of zeros.
//!
//! # The decode is `akuma-syscalls-mem`, not local
//!
//! Which *kind* of mapping a request asks for — anonymous or file-backed, lazy
//! or eager, shared-writable, a `PROT_NONE` reservation — is decided by
//! [`akuma_syscalls_mem::mmap::plan`], and `MAP_FIXED`'s alignment rule by
//! `fixed_addr_unaligned_einval`. That crate is a pure function of the argument
//! bits with host tests and seven pinned divergences from Linux, and it builds
//! for `x86_64-unknown-none` unchanged.
//!
//! This module first hand-rolled that decode, which was wrong twice over: it
//! duplicated tested logic, and it would have drifted from the AArch64 kernel's
//! behaviour on exactly the arguments where Linux compatibility is subtle. What
//! stays here is the half that is genuinely per-architecture — allocating
//! frames and writing page tables.
//!
//! `akuma-mmap` is the *other* half, the region bookkeeping, and it is
//! deliberately not used yet: its `MmapRegion.flags` is a raw AArch64 PTE `u64`
//! (`docs/archive/REDUCING_PLATFORM_DEPENDENCY.md` §1, still open) and the two
//! encodings share no field. Adopting it needs item 1 first, which is the
//! prerequisite the proposal always said it was. Until then the only thing this
//! module gives up is `munmap`'s clip-and-split and lazy regions.

use crate::paging::{self, MemAttr, Prot};
use crate::phys::phys_ptr;
use akuma_selftest::Suite;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::fd::errno;

const PAGE_SIZE: u64 = 4096;

/// `PROT_WRITE` / `PROT_EXEC`, from the shared flag tables rather than restated
/// here — the same constants the AArch64 kernel dispatches on.
use akuma_syscalls_linux::flags::prot::{PROT_EXEC, PROT_WRITE};

/// How many pages this target maps eagerly before `plan` calls a mapping lazy.
///
/// `usize::MAX`, because there is no lazy path: without a region table the `#PF`
/// handler cannot know which faults to service, so every mapping must be
/// populated at `mmap` time. Passing the real `MMAP_EAGER_MAX_PAGES` would make
/// `plan` return `use_lazy` for a large request and this module would then have
/// to refuse it — saying "never lazy" up front is the honest spelling.

/// Where anonymous mappings start.
///
/// Well above where a static binary is linked (`0x40_0000`) and well below the
/// stack (`0x7FFF_FFFF_F000`), so a growing heap and a growing stack cannot meet
/// without both being enormous. A bump allocator rather than a free list: this
/// never reuses an address, which makes a use-after-munmap fault instead of
/// silently landing in someone else's mapping.
const MMAP_BASE: u64 = 0x1_0000_0000;
static NEXT_VA: AtomicU64 = AtomicU64::new(MMAP_BASE);

/// Largest single mapping. A bound rather than trust: the length comes from
/// ring 3, and an unbounded one would loop allocating frames until the PMM is
/// empty and the kernel is unrecoverable.
const MAX_MAPPING: u64 = 64 * 1024 * 1024;

/// See the note on the `prot` import: never lazy, because there is no lazy path.
const EAGER_MAX_PAGES: usize = usize::MAX;

/// `mmap(addr, len, prot, flags, fd)`.
pub fn sys_mmap(addr: u64, len: u64, prot: u64, flags: u64, fd: u64) -> u64 {
    if len == 0 || len > MAX_MAPPING {
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

    let pages = len.div_ceil(PAGE_SIZE);
    let plan = akuma_syscalls_mem::mmap::plan(prot32, flags32, fd32, pages as usize, EAGER_MAX_PAGES);

    if plan.is_file_backed {
        // Refused rather than served as anonymous memory: see the module header.
        return errno::ENOSYS;
    }
    if flags32 & akuma_syscalls_linux::flags::map::MAP_FIXED != 0 {
        return errno::ENOSYS;
    }
    let _ = addr; // Without MAP_FIXED an address is a hint, and hints are advisory.

    // W^X, enforced here as it is in the ELF loader: `Prot` offers no
    // writable-and-executable constructor, and a JIT is not something this
    // target supports.
    if prot32 & PROT_WRITE != 0 && prot32 & PROT_EXEC != 0 {
        return errno::EINVAL;
    }
    let page_prot = if prot32 & PROT_EXEC != 0 {
        Prot::USER_RX
    } else {
        Prot::USER_RW
    };

    let base = NEXT_VA.fetch_add(pages * PAGE_SIZE, Ordering::Relaxed);

    let root = paging::active_root();
    for i in 0..pages {
        let Some(frame) = akuma_pmm::alloc_page() else {
            // Out of memory partway through. The pages already mapped are left
            // mapped and leaked: unwinding them correctly needs the region table
            // this module does not have, and unmapping a range the caller was
            // never told about is the more dangerous of the two mistakes.
            return errno::EINVAL;
        };
        // Zero before mapping: a recycled frame otherwise hands ring 3 whatever
        // the previous owner left in it. The same rule the demand-paging handler
        // and the ELF loader follow.
        // SAFETY: a fresh PMM frame, reached through the physmap.
        unsafe { core::ptr::write_bytes(phys_ptr::<u8>(frame as u64), 0, PAGE_SIZE as usize) };
        let va = base + i * PAGE_SIZE;
        if !paging::map_page_in(root, va as usize, frame as u64, page_prot, MemAttr::WriteBack) {
            akuma_pmm::free_page(frame, 0);
            return errno::EINVAL;
        }
    }
    base
}

/// `munmap(addr, len)`.
///
/// Unmaps and frees. The address space is not reclaimed — [`NEXT_VA`] never goes
/// backwards — so this returns frames to the PMM without making the range
/// reusable, which is what stops a use-after-free landing in a later mapping.
pub fn sys_munmap(addr: u64, len: u64) -> u64 {
    if len == 0 || len > MAX_MAPPING || !addr.is_multiple_of(PAGE_SIZE) {
        return errno::EINVAL;
    }
    let root = paging::active_root();
    for i in 0..len.div_ceil(PAGE_SIZE) {
        let va = (addr + i * PAGE_SIZE) as usize;
        if let Some(frame) = paging::unmap_page_in(root, va) {
            akuma_pmm::free_page(frame as usize, 0);
        }
    }
    0
}

/// Check the refusals, which are the part a guest program cannot easily reach.
///
/// The success path is exercised for real by every allocating program; what is
/// worth testing here is that the things this does *not* support fail rather
/// than appearing to work.
pub fn smoke_test(t: &mut Suite) {
    use akuma_syscalls_linux::flags::map::{MAP_ANONYMOUS, MAP_FIXED};
    const ANON: u64 = MAP_ANONYMOUS as u64;
    // -1, the "no file" sentinel, as it arrives from a 64-bit register.
    const NO_FD: u64 = u64::MAX;

    t.check_eq("mmap: zero length is EINVAL", sys_mmap(0, 0, 3, ANON, NO_FD), errno::EINVAL);
    t.check_eq(
        "mmap: an oversized request is EINVAL",
        sys_mmap(0, MAX_MAPPING + 1, 3, ANON, NO_FD),
        errno::EINVAL,
    );
    t.check_eq(
        "mmap: a file-backed request is refused",
        sys_mmap(0, 4096, 3, 0, 5),
        errno::ENOSYS,
    );
    t.check_eq(
        "mmap: MAP_FIXED is refused",
        sys_mmap(0x5000_0000, 4096, 3, ANON | MAP_FIXED as u64, NO_FD),
        errno::ENOSYS,
    );
    t.check_eq(
        "mmap: writable+executable is refused",
        sys_mmap(0, 4096, u64::from(PROT_WRITE | PROT_EXEC), ANON, NO_FD),
        errno::EINVAL,
    );
    t.check_eq(
        "munmap: an unaligned address is EINVAL",
        sys_munmap(0x1001, 4096),
        errno::EINVAL,
    );
}

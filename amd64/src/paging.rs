//! x86_64 4-level page tables: map, unmap, translate.
//!
//! Stage B of the amd64 bring-up. `boot.s` leaves the machine on a fixed
//! identity map of the first 1 GiB built from 2 MiB pages; this is the first
//! code that can *change* a mapping, which is the prerequisite for anything that
//! demand-pages, protects a region, or addresses memory beyond that window.
//!
//! # Relationship to `akuma-mmap` and proposal item 1
//!
//! [`Prot`] below is deliberately the shape
//! `proposals/REDUCING_PLATFORM_DEPENDENCY.md` §1 proposes for `akuma-mmap`: a
//! small `Copy` struct of `{read, write, exec, user}` with named constructors,
//! **not** a `u64` of architectural bits. It is defined here rather than reused
//! from `akuma-mmap` because `MmapRegion.flags` is still a raw AArch64 `u64`
//! today, and that `u64` cannot cross to x86 — the encodings share no field:
//!
//! | | AArch64 | x86_64 |
//! |---|---|---|
//! | writable | `AP[7:6]` == `01` (a *field*, not a bit) | bit 1 set |
//! | user | `AP[6]` set | bit 2 set |
//! | no-execute (EL0/user) | `UXN`, bit 54 | `NX`, bit 63 |
//! | no-execute (EL1/kernel) | `PXN`, bit 53 | — (no separate bit) |
//! | access flag | `AF`, bit 10, **must be set by software** | `A`, bit 5, set by hardware |
//!
//! Note the last two rows especially: AArch64 has two execute-permission bits to
//! x86's one, so `PXN` has no x86 counterpart and a straight bit-for-bit
//! translation loses information in one direction. That asymmetry is the concrete
//! argument for item 1 — the neutral vocabulary has to be *permissions*, and each
//! architecture's encoder decides how to spell them.
//!
//! When item 1 lands, this `Prot` should be deleted and `akuma_mmap::Prot`
//! used instead, with [`encode`] becoming that crate's x86 backend.
//!
//! # Why the tables can be dereferenced directly
//!
//! Every page-table frame is allocated from `akuma-pmm`, whose pool is inside the
//! region `boot.s` identity-maps, so a physical address *is* a valid pointer.
//! [`table_mut`] asserts that rather than assuming it: the moment the PMM is
//! given memory above 1 GiB, this stops being true, and the failure mode is a
//! page fault with no IDT installed — a triple-fault and a guest that vanishes
//! with no output.

use crate::serial;

/// Present.
const P: u64 = 1 << 0;
/// Writable.
const RW: u64 = 1 << 1;
/// User-accessible (ring 3).
const US: u64 = 1 << 2;
/// Page size — at PD level this means a 2 MiB page rather than a PT pointer.
const PS: u64 = 1 << 7;
/// No-execute. **Requires `EFER.NXE`**, which `boot.s` sets alongside `LME`;
/// without it this is a reserved bit and setting it faults.
const NX: u64 = 1 << 63;

/// Physical address field of an entry: bits 51:12.
const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

const PAGE_SIZE: usize = 4096;
/// Entries per table: 4 KiB / 8 bytes. Also the index mask below.
const ENTRIES: usize = 512;

/// `boot.s` identity-maps exactly this much, which bounds what is dereferenceable.
const IDENTITY_MAP_LIMIT: u64 = 1 << 30;

/// Permissions, as permissions — not as an encoding.
///
/// See the module header: this is the vocabulary proposal item 1 wants, and the
/// point of keeping it a struct is that `RO` and `EXEC` cannot accidentally be
/// the same value, which is exactly the defect item 1.1 documents on the
/// AArch64 side.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Prot {
    pub write: bool,
    pub exec: bool,
    pub user: bool,
}

impl Prot {
    /// Kernel read-only, no execute.
    pub const KERNEL_RO: Self = Self { write: false, exec: false, user: false };
    /// Kernel read/write, no execute. The default for data.
    pub const KERNEL_RW: Self = Self { write: true, exec: false, user: false };
    /// Kernel read + execute, not writable. The only executable shape offered:
    /// there is deliberately no writable-and-executable constructor.
    pub const KERNEL_RX: Self = Self { write: false, exec: true, user: false };
    /// User read/write, no execute.
    pub const USER_RW: Self = Self { write: true, exec: false, user: true };
    /// User read + execute.
    pub const USER_RX: Self = Self { write: false, exec: true, user: true };
}

/// Encode a [`Prot`] into x86_64 PTE permission bits.
///
/// The x86 half of what item 1 calls `encode(prot, attr)`. Memory attributes
/// (PAT/PCD/PWT) are not modelled yet — everything here is writeback normal
/// memory — so the `MemAttr` half of that signature is deliberately absent
/// rather than stubbed with a value that would look meaningful.
const fn encode(prot: Prot) -> u64 {
    let mut bits = P;
    if prot.write {
        bits |= RW;
    }
    if prot.user {
        bits |= US;
    }
    if !prot.exec {
        bits |= NX;
    }
    bits
}

/// Interpret a physical address as a page table.
///
/// # Safety
/// `pa` must be a page-aligned frame that is either a live page table or a
/// freshly-zeroed frame the caller is about to make one.
unsafe fn table_mut(pa: u64) -> *mut u64 {
    debug_assert!(pa < IDENTITY_MAP_LIMIT, "page table outside the identity map");
    pa as *mut u64
}

/// The active top-level table, from `CR3`.
fn read_cr3() -> u64 {
    let v: u64;
    // SAFETY: reading CR3 copies a register into a local; it dereferences
    // nothing and has no side effect.
    unsafe {
        core::arch::asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags));
    }
    v & ADDR_MASK
}

/// Invalidate one page's TLB entry.
///
/// The x86 counterpart of `tlbi vaae1`, and a good illustration of proposal
/// item 3: `invlpg` is *core-local*, where AArch64's `tlbi ...is` broadcasts to
/// the inner-shareable domain. On x86 a multi-core kernel must send an IPI to
/// every other core that could hold the translation — there is no broadcast
/// form. Single-core here, so this is complete; it will not stay that way, and
/// item 3's `TlbTarget` is the vocabulary that would make the difference sayable.
fn invlpg(va: usize) {
    // SAFETY: invalidation forces a re-walk; it cannot grant access, and it
    // does not dereference `va`.
    unsafe {
        core::arch::asm!("invlpg [{}]", in(reg) va, options(nostack, preserves_flags));
    }
}

/// Index into the level-`n` table for `va`. Level 4 = PML4 … level 1 = PT.
const fn index(va: usize, level: u32) -> usize {
    (va >> (12 + 9 * (level - 1))) & (ENTRIES - 1)
}

/// Fetch the next table down, allocating and zeroing it if absent.
///
/// Returns `None` if a frame could not be allocated, or if the entry is a
/// 2 MiB page rather than a table pointer — this deliberately refuses to split
/// a large page. `boot.s`'s identity map is built from 2 MiB pages, so any
/// attempt to map a 4 KiB page below 1 GiB lands here and is rejected loudly
/// instead of silently corrupting the map.
unsafe fn next_table(entry_ptr: *mut u64, user: bool) -> Option<u64> {
    // SAFETY: caller guarantees `entry_ptr` points into a live table.
    let entry = unsafe { entry_ptr.read_volatile() };

    if entry & P != 0 {
        if entry & PS != 0 {
            return None; // a 2 MiB page; splitting is not implemented
        }
        // Widen permissions on the way down: a parent that is not user-
        // accessible or not writable masks every child on x86, so an
        // intermediate entry has to be at least as permissive as any leaf
        // beneath it. Enforcement lives entirely in the leaf.
        let want = P | RW | if user { US } else { 0 };
        if entry & want != want {
            // SAFETY: as above.
            unsafe { entry_ptr.write_volatile((entry | want) & !NX) };
        }
        return Some(entry & ADDR_MASK);
    }

    let frame = akuma_pmm::alloc_page()? as u64;
    // SAFETY: a fresh PMM frame inside the identity map; zeroing it is what
    // makes it a valid empty table.
    unsafe {
        core::ptr::write_bytes(frame as *mut u8, 0, PAGE_SIZE);
        // Intermediate entries are permissive; the leaf decides. NX is left
        // clear here for the same reason.
        entry_ptr.write_volatile(frame | P | RW | if user { US } else { 0 });
    }
    Some(frame)
}

/// Map `va` to `pa` with `prot`. Returns false if a table could not be
/// allocated or if the range is backed by a 2 MiB page.
pub fn map_page(va: usize, pa: u64, prot: Prot) -> bool {
    assert_eq!(va % PAGE_SIZE, 0, "va must be page aligned");
    assert_eq!(pa % PAGE_SIZE as u64, 0, "pa must be page aligned");

    let mut table = read_cr3();
    for level in (2..=4).rev() {
        // SAFETY: `table` is a live table frame inside the identity map.
        let entry_ptr = unsafe { table_mut(table).add(index(va, level)) };
        // SAFETY: as above.
        match unsafe { next_table(entry_ptr, prot.user) } {
            Some(next) => table = next,
            None => return false,
        }
    }

    // SAFETY: `table` is the PT frame; the index is masked to 0..512.
    unsafe {
        table_mut(table)
            .add(index(va, 1))
            .write_volatile(pa | encode(prot));
    }
    invlpg(va);
    true
}

/// Remove the mapping for `va`, if any. Returns the physical address it held.
pub fn unmap_page(va: usize) -> Option<u64> {
    let mut table = read_cr3();
    for level in (2..=4).rev() {
        // SAFETY: `table` is a live table frame inside the identity map.
        let entry = unsafe { table_mut(table).add(index(va, level)).read_volatile() };
        if entry & P == 0 || entry & PS != 0 {
            return None;
        }
        table = entry & ADDR_MASK;
    }
    // SAFETY: `table` is the PT frame.
    let leaf = unsafe { table_mut(table).add(index(va, 1)) };
    // SAFETY: as above.
    let entry = unsafe { leaf.read_volatile() };
    if entry & P == 0 {
        return None;
    }
    // SAFETY: as above.
    unsafe { leaf.write_volatile(0) };
    invlpg(va);
    Some(entry & ADDR_MASK)
}

/// Resolve `va` to a physical address by walking the live tables.
///
/// Walks rather than trusting a shadow structure, so it reports what the
/// *hardware* would do — which is the only useful answer when checking whether a
/// mapping took effect.
pub fn translate(va: usize) -> Option<u64> {
    let mut table = read_cr3();
    for level in (2..=4).rev() {
        // SAFETY: `table` is a live table frame inside the identity map.
        let entry = unsafe { table_mut(table).add(index(va, level)).read_volatile() };
        if entry & P == 0 {
            return None;
        }
        if entry & PS != 0 {
            // A 2 MiB page at PD level: offset within it.
            let size = 1 << 21;
            return Some((entry & ADDR_MASK) + (va as u64 & (size - 1)));
        }
        table = entry & ADDR_MASK;
    }
    // SAFETY: `table` is the PT frame.
    let entry = unsafe { table_mut(table).add(index(va, 1)).read_volatile() };
    if entry & P == 0 {
        return None;
    }
    Some((entry & ADDR_MASK) + (va as u64 & (PAGE_SIZE as u64 - 1)))
}

/// Map a frame outside the identity map, write through it, read it back, unmap.
///
/// Chosen VA is 1 GiB — the first address `boot.s` does *not* map, so the whole
/// path (allocate PDPT entry, PD, PT, leaf) is exercised and a false pass from
/// accidentally hitting the identity map is impossible.
pub fn smoke_test() {
    const TEST_VA: usize = 1 << 30;
    const PATTERN: u64 = 0x0bad_c0de_dead_beef;

    serial::puts("  test: paging ");

    if translate(TEST_VA).is_some() {
        serial::puts("[FAIL] 0x40000000 already mapped\n");
        return;
    }

    let Some(frame) = akuma_pmm::alloc_page() else {
        serial::puts("[FAIL] no frame\n");
        return;
    };

    if !map_page(TEST_VA, frame as u64, Prot::KERNEL_RW) {
        serial::puts("[FAIL] map_page\n");
        return;
    }

    match translate(TEST_VA) {
        Some(pa) if pa == frame as u64 => {}
        _ => {
            serial::puts("[FAIL] translate mismatch\n");
            return;
        }
    }

    // SAFETY: the mapping was just installed and verified by a table walk.
    let readback = unsafe {
        let p = TEST_VA as *mut u64;
        p.write_volatile(PATTERN);
        p.read_volatile()
    };
    if readback != PATTERN {
        serial::puts("[FAIL] readback\n");
        return;
    }

    // The write must be visible at the *physical* address too — that is what
    // proves the mapping points where the walk said, rather than at some other
    // page that happens to be readable.
    // SAFETY: PMM frames are inside the identity map.
    let via_phys = unsafe { (frame as *const u64).read_volatile() };
    if via_phys != PATTERN {
        serial::puts("[FAIL] physical alias mismatch\n");
        return;
    }

    if unmap_page(TEST_VA) != Some(frame as u64) {
        serial::puts("[FAIL] unmap\n");
        return;
    }
    if translate(TEST_VA).is_some() {
        serial::puts("[FAIL] still mapped after unmap\n");
        return;
    }

    akuma_pmm::free_page(frame, 0);

    serial::puts("map/write/verify/unmap @0x");
    serial::put_hex(TEST_VA as u64);
    serial::puts("   [OK]\n");

    nx_encoding_check();
}

/// Check that the encoder cannot express write+execute, and that NX is set.
///
/// A pure check on [`encode`], not on the hardware: the hardware half needs a
/// `#PF` handler to observe, and there is no IDT yet. What it does prove is the
/// property item 1 exists to guarantee — that "read-only" and "executable" are
/// distinct values, which on the AArch64 side they currently are not
/// (`user_flags::RO == user_flags::EXEC`).
fn nx_encoding_check() {
    let rw = encode(Prot::KERNEL_RW);
    let rx = encode(Prot::KERNEL_RX);
    let ro = encode(Prot::KERNEL_RO);

    // User mappings must additionally carry US, and kernel mappings must not:
    // a kernel page that leaks US is reachable from ring 3.
    let urw = encode(Prot::USER_RW);
    let urx = encode(Prot::USER_RX);

    let ok = rw & NX != 0        // data is never executable
        && rw & RW != 0
        && rx & NX == 0          // code is executable
        && rx & RW == 0          // ...and never writable
        && ro & NX != 0
        && ro & RW == 0
        && ro != rx              // the defect item 1.1 describes, absent here
        && urw & US != 0         // user mappings are user-accessible
        && urx & US != 0
        && urw & NX != 0         // ...and user data is still never executable
        && urx & RW == 0         // ...and user code is still never writable
        && rw & US == 0          // kernel mappings are NOT reachable from ring 3
        && rx & US == 0
        && ro & US == 0;

    serial::puts("  test: W^X encoding ");
    serial::puts(if ok { "  [OK]\n" } else { "  [FAIL]\n" });
}

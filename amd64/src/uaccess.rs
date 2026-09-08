//! Fault-safe access to user memory, on this target.
//!
//! Every syscall body used to dereference user pointers raw (`read_volatile`
//! through `ptr as *const u8`), with a comment saying a bad pointer "faults
//! reportably" — meaning the kernel halted with a register dump. That was the
//! honest limit before 2026-09-05; since then `akuma-user-access`'s
//! `copy_from_user_safe` has a real x86_64 arm, and `idt.rs`'s page-fault path
//! turns a fault inside its copy loop into a returned `EFAULT`
//! (`docs/archive/AKUMA_USER_ACCESS_X86_FIXUP.md`). This module is the thin
//! layer the syscall bodies go through to reach it, so that a program passing
//! garbage — which every real program eventually does — costs it an errno and
//! not the machine.
//!
//! # What the check adds on top of the copy
//!
//! The copy recovers a *page fault*. A **non-canonical** address (bit 47 not
//! sign-extended) raises `#GP`, which `idt.rs` also fixes up when the `rip` is
//! in the loop, but the cheap answer is to never get there: [`range_ok`]
//! rejects everything at or above `0x0000_8000_0000_0000`, plus the null page,
//! plus any length that would wrap. That is `akuma-user-access`'s
//! `user_range_ok` with the x86_64 canonical bound, which is also what
//! `USER_VA_LIMIT` now is on this target.
//!
//! There is no "is it mapped" walk here and no prefault: this target has no
//! lazy user regions yet, so the copy either succeeds or faults, and the fault
//! is recovered. When lazy regions arrive, the walk goes here.
//!
//! # The boot self-tests, and `BYPASS_VALIDATION`
//!
//! `fd::smoke_test`, `sock::smoke_test` and the spawn/exec/fork tests drive
//! the syscall bodies directly with **kernel-stack buffers** where a program
//! would pass user pointers. Those are kernel addresses, which [`range_ok`]
//! rightly refuses — so the range check honours
//! `akuma_user_access::BYPASS_VALIDATION`, the per-thread switch the AArch64
//! kernel's ~85 boot-test sites already use for exactly this, and `main.rs`
//! holds a `BypassValidationGuard` across the self-test block and drops it
//! before `run_init`. Under the bypass the *copy* is still fault-safe (an
//! unmapped kernel address is recovered like any other); only the "is this in
//! the user half" question is waived.
//!
//! # SMAP: the hardware half (2026-09-05)
//!
//! With `CR4.SMAP` set, a ring-0 access to a user-accessible page faults unless
//! `RFLAGS.AC` is set — so the *only* kernel code that can touch user memory is
//! code that says so, and a bug in [`range_ok`] (or a kernel path that still
//! dereferences a user pointer raw) becomes a fault instead of a silent read.
//! [`read_bytes`]/[`write_bytes`] bracket the copy in `stac`/`clac`; nothing
//! else in the kernel sets `AC`. Two entry paths have to clear it: the
//! exception stubs in `idt.rs` (hardware does **not** clear `AC` on interrupt
//! delivery — Linux's `idtentry` executes `ASM_CLAC` for the same reason), and
//! `syscall`, via bit 18 in `IA32_FMASK`, so a program cannot enter the kernel
//! with `AC` already set. `iretq` restores it, which is what lets a faulting
//! `rep movsb` be demand-paged and re-executed mid-copy.
//!
//! Enabled only if `CPUID.7.0:EBX[20]` says so — `stac`/`clac` are `#UD`
//! otherwise — which is not academic: Haswell (the HP 500-502nj's i5-4460) has
//! SMEP but **not** SMAP. `SMEP` (`EBX[7]`) is turned on alongside; the kernel
//! never executes from a user page. [`SMAP_ACTIVE`] is what every `stac`/`clac`
//! site consults.

//! # Values, not just bytes
//!
//! [`read_val`]/[`write_val`] exist because the syscall ABI is full of small
//! fixed-layout structs — `timespec`, `iovec`, `pollfd` fields — that were each
//! being read as two or three separate raw loads. `T` must be plain ABI data
//! (integers and arrays of them): user bytes land on it verbatim, and this
//! module cannot check that for you.

use alloc::vec::Vec;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU8, Ordering};

use akuma_selftest::Suite;

/// Non-zero once [`init_smap`] has set `CR4.SMAP`. `#[no_mangle]` because the
/// exception stubs in `idt.rs` test it as `[rip + SMAP_ACTIVE]` before their
/// `clac`, which would `#UD` on a CPU without SMAP.
#[unsafe(no_mangle)]
pub static SMAP_ACTIVE: AtomicU8 = AtomicU8::new(0);

/// What CPUID advertised, for the boot line and the self-test. What was
/// actually *set* is read back from `CR4` ([`cr4_bits`]) rather than echoed, so
/// the test compares two independent sources.
#[derive(Clone, Copy)]
pub struct SmapStatus {
    pub cpuid_smap: bool,
    pub cpuid_smep: bool,
}

const CR4_SMEP: u64 = 1 << 20;
const CR4_SMAP: u64 = 1 << 21;
/// `CR0.WP` — **write protect**: make ring 0 honour the `R/W` bit of a page.
///
/// With `WP` clear (the reset state, and what this kernel booted with until
/// 2026-09-07) a supervisor write succeeds against **any** mapped page whatever
/// its `R/W` bit says. That is not a missing hardening, it is a live
/// correctness hole, and copy-on-write is where it bites:
///
/// `copy_to_user` writes through the *user* virtual address with `rep movsb`
/// from ring 0. After a `fork` the child's pages are mapped read-only and
/// CoW-marked. With `WP` clear a `read(2)` into a buffer the child has not
/// written since the fork takes **no fault at all** — so the CoW break never
/// runs, and the bytes land in the frame the **parent** is still mapping. The
/// parent's memory changes underneath it, silently, with no error anywhere.
///
/// `idt::page_fault_dispatch` had the arm for this and could never reach it: it
/// required the fault to come from ring 3 (`PageFaultCode::is_user_mode`), and
/// a `copy_to_user` fault comes from ring 0. Its own comment described the
/// behaviour this bit is what actually produces — see the CoW arm there.
///
/// Architectural since the 486 and unconditional in long mode, so unlike
/// SMAP/SMEP there is nothing to ask CPUID about.
const CR0_WP: u64 = 1 << 16;
/// `RFLAGS.AC`.
const RFLAGS_AC: u64 = 1 << 18;

/// Turn on `CR4.SMAP`/`CR4.SMEP` where CPUID advertises them, and `CR0.WP`
/// unconditionally.
///
/// The three belong together: each one makes a ring-0 access to a user page
/// obey a rule it was previously exempt from. SMAP requires the access to be
/// *declared* (`stac`), SMEP forbids *executing* one, and [`CR0_WP`] makes a
/// *write* honour the page's `R/W` bit — which is what routes a `copy_to_user`
/// onto a CoW page through the break instead of straight into a frame the
/// parent still shares.
///
/// Called on **every** core (the BSP from `kmain`, each AP from `ap_entry64`),
/// which is why this is the right home for `WP`: all three are per-core
/// registers, and a secondary that missed any of them would enforce a different
/// rule than the boot core.
///
/// Call once, early — before the first syscall and before the self-tests, so
/// everything after runs under the enforcement it will ship with. Safe to call
/// with paging up: the bits change how *future* accesses are checked and
/// nothing in this kernel touches a user page without [`read_bytes`] /
/// [`write_bytes`] (the loader and the fork/exec page copies go through the
/// physmap, which is supervisor-only — and therefore writable regardless of
/// `WP`, since `WP` only governs the `R/W` bit and those mappings are `R/W`).
pub fn init_smap() -> SmapStatus {
    // Leaf 7 subleaf 0's `EBX`: bit 7 SMEP, bit 20 SMAP.
    //
    // Through `core::arch::x86_64::__cpuid_count`, **not** a hand-rolled `asm!`
    // template, because getting `EBX` back out of `cpuid` is the one genuinely
    // hard part of this: `rbx` is callee-saved and LLVM reserves it, so naming
    // it as a clobber is a compile error and it has to be saved and restored
    // inside the template — and both hand-written shapes this file carried were
    // wrong, in opposite directions, with nothing red either time.
    //
    // 1. `mov {tmp:r}, rbx` / `cpuid` / `mov {ebx:e}, ebx` / `mov rbx, {tmp:r}`
    //    with two `out(reg)` operands. LLVM may allocate the *result* operand
    //    to `rbx` itself, and then the restore overwrites the result: the
    //    function returned whatever the caller's `rbx` happened to hold.
    // 2. The 2026-09-08 repair stashed the result into the save slot instead —
    //    `mov {tmp:e}, ebx` / `mov rbx, {tmp:r}`. That reads the right value,
    //    but it destroys the saved `rbx` on the way and then "restores" `rbx`
    //    *from the result*. Verified in the shipped image: `init_smap` returned
    //    with `rbx` = the CPUID feature word, while LLVM believed `rbx` had
    //    survived the call — `ap_entry64` keeps the AP's cpu index there across
    //    it and printed the feature word as the cpu number.
    //
    // The intrinsic uses the `xchg` form, which is correct for *every*
    // allocation including the degenerate one where the operand is `rbx`: the
    // result lands in `rbx` and LLVM knows it, because LLVM chose it.
    //
    // Whether SMAP was "detected" therefore used to depend on register
    // allocation, i.e. on code layout — the same CPU answered `on` on one build
    // and `off` on the next, and the `CR4.SMAP follows CPUID` self-test passed
    // both times because both sides read the same corrupted word. A self-test
    // that compares a register against the value that programmed it cannot see
    // this class of bug; the check that would have is a CPUID word compared
    // against a second source.
    //
    // The intrinsic is *safe* to call: `cpuid` is unprivileged,
    // side-effect-free, and baseline on x86_64, so no feature detection is
    // needed to run the feature detection — which is the second reason to
    // prefer it here, in a file that is otherwise all `unsafe`.
    let ebx = core::arch::x86_64::__cpuid_count(7, 0).ebx;

    let cpuid_smep = ebx & (1 << 7) != 0;
    let cpuid_smap = ebx & (1 << 20) != 0;

    let mut cr4: u64;
    // SAFETY: reading CR4 has no side effect.
    unsafe { core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags)) };
    if cpuid_smep {
        cr4 |= CR4_SMEP;
    }
    if cpuid_smap {
        cr4 |= CR4_SMAP;
    }
    // SAFETY: only the two feature bits CPUID just confirmed are added; every
    // other bit is written back as read. Enabling SMEP/SMAP changes the
    // permission check on user pages for ring 0 and nothing else.
    unsafe { core::arch::asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack, preserves_flags)) };
    if cpuid_smap {
        SMAP_ACTIVE.store(1, Ordering::Release);
        // The shared user-copy crate has its own copy loop (`rep movsb`) and
        // its own flag byte; it must be told too, or every copy through
        // `akuma-syscalls-glue` faults on a legitimate user page. Two flags
        // rather than one because the crate cannot name this kernel's static —
        // set together, here, so they cannot disagree.
        akuma_user_access::set_smap_active(true);
    }

    let mut cr0: u64;
    // SAFETY: reading CR0 has no side effect.
    unsafe { core::arch::asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags)) };
    cr0 |= CR0_WP;
    // SAFETY: one bit added, every other written back as read. From here a
    // ring-0 write to a read-only page faults instead of succeeding — which the
    // `#PF` handler services (a CoW page) or reports (anything else). Nothing
    // in this kernel writes a user page except through `write_bytes` below, and
    // its `rep movsb` is already fixup-covered.
    unsafe { core::arch::asm!("mov cr0, {}", in(reg) cr0, options(nomem, nostack, preserves_flags)) };

    SmapStatus { cpuid_smap, cpuid_smep }
}

/// Is `CR0.WP` set right now? Read back from the register, never echoed from
/// what was written — the same discipline as [`cr4_bits`], and the reason the
/// self-test can tell "we set it" from "the CPU has it".
#[must_use]
pub fn write_protect_on() -> bool {
    let cr0: u64;
    // SAFETY: reading CR0 has no side effect.
    unsafe { core::arch::asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags)) };
    cr0 & CR0_WP != 0
}

/// `(CR4.SMAP, CR4.SMEP)` as the CPU currently has them.
#[must_use]
pub fn cr4_bits() -> (bool, bool) {
    let cr4: u64;
    // SAFETY: reading CR4 has no side effect.
    unsafe { core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags)) };
    (cr4 & CR4_SMAP != 0, cr4 & CR4_SMEP != 0)
}

/// Allow ring-0 user-page access until [`clac`]. A no-op when SMAP is off, where
/// the instruction would `#UD`.
#[inline]
fn stac() {
    if SMAP_ACTIVE.load(Ordering::Relaxed) != 0 {
        // SAFETY: sets `RFLAGS.AC`; that is its whole effect, and it is paired
        // with `clac` on every path out of the copy below.
        unsafe { core::arch::asm!("stac", options(nomem, nostack)) };
    }
}

/// End a [`stac`] window.
#[inline]
fn clac() {
    if SMAP_ACTIVE.load(Ordering::Relaxed) != 0 {
        // SAFETY: clears `RFLAGS.AC`; nothing else.
        unsafe { core::arch::asm!("clac", options(nomem, nostack)) };
    }
}

/// For the `x86-interrupt` handlers `idt.rs` cannot prefix with asm: clear `AC`
/// on entry, so a tick landing mid-copy does not run the scheduler with SMAP
/// suspended. The hand-assembled vectors 13/14 do this in their stubs.
#[inline]
pub fn clac_if_enabled() {
    clac();
}

/// Current `RFLAGS.AC`, for the self-test.
fn ac_set() -> bool {
    let flags: u64;
    // SAFETY: pushes and pops one word on the current stack.
    unsafe { core::arch::asm!("pushfq", "pop {}", out(reg) flags, options(nomem, preserves_flags)) };
    flags & RFLAGS_AC != 0
}

use akuma_user_access::{copy_from_user_safe, copy_to_user_safe};

/// First address the user half cannot reach: the x86_64 canonical boundary.
pub const USER_END: u64 = 0x0000_8000_0000_0000;

/// Is `[ptr, ptr + len)` a range a user program could legitimately hand over?
///
/// Null page rejected (a NULL-plus-small-offset is the most common garbage
/// pointer there is), wrap rejected, kernel half and non-canonical rejected. A
/// zero-length range is judged on its pointer alone, as `user_range_ok` does.
#[must_use]
pub const fn range_ok(ptr: u64, len: u64) -> bool {
    if ptr < 0x1000 {
        return false;
    }
    match ptr.checked_add(len) {
        Some(end) => end <= USER_END,
        None => false,
    }
}

/// [`range_ok`], or the calling thread is inside a `BYPASS_VALIDATION` window
/// (boot self-tests only — see the module note). A wrapping range is refused
/// even under the bypass: nothing legitimate wraps.
fn range_ok_or_bypassed(ptr: u64, len: u64) -> bool {
    if range_ok(ptr, len) {
        return true;
    }
    akuma_user_access::BYPASS_VALIDATION.load(Ordering::Acquire) && ptr.checked_add(len).is_some()
}

/// Copy `dst.len()` bytes in from user address `ptr`. `false` on a bad range
/// or a fault; `dst` may then be partly written.
#[must_use]
pub fn read_bytes(ptr: u64, dst: &mut [u8]) -> bool {
    if dst.is_empty() {
        return true;
    }
    if !range_ok_or_bypassed(ptr, dst.len() as u64) {
        return false;
    }
    stac();
    // SAFETY: `dst` is a live kernel slice of the stated length; the source is
    // a range-checked user address, and a fault in the copy is recovered by the
    // page-fault handler into an `Err` rather than a halt.
    let ok = unsafe { copy_from_user_safe(dst.as_mut_ptr(), ptr as *const u8, dst.len()) }.is_ok();
    clac();
    ok
}

/// Copy `src` out to user address `ptr`. `false` on a bad range or a fault; the
/// destination may then be partly written.
#[must_use]
pub fn write_bytes(ptr: u64, src: &[u8]) -> bool {
    if src.is_empty() {
        return true;
    }
    if !range_ok_or_bypassed(ptr, src.len() as u64) {
        return false;
    }
    stac();
    // SAFETY: as `read_bytes`, with the roles swapped.
    let ok = unsafe { copy_to_user_safe(ptr as *mut u8, src.as_ptr(), src.len()) }.is_ok();
    clac();
    ok
}

/// Read one plain-ABI value from user memory.
///
/// `T` must be integers or arrays of integers — see the module note. Alignment
/// is not required of `ptr`: the copy is byte-granular.
#[must_use]
pub fn read_val<T: Copy>(ptr: u64) -> Option<T> {
    let mut slot = MaybeUninit::<T>::uninit();
    // SAFETY: `size_of::<T>()` writable bytes at the slot's address.
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(slot.as_mut_ptr().cast::<u8>(), core::mem::size_of::<T>())
    };
    if !read_bytes(ptr, bytes) {
        return None;
    }
    // SAFETY: every byte was written by the copy, and the caller's `T` is plain
    // ABI data for which any byte pattern is a value.
    Some(unsafe { slot.assume_init() })
}

/// Write one plain-ABI value to user memory. Same `T` requirement as
/// [`read_val`].
#[must_use]
pub fn write_val<T: Copy>(ptr: u64, v: T) -> bool {
    // SAFETY: `size_of::<T>()` readable bytes of a live local.
    let bytes = unsafe {
        core::slice::from_raw_parts((&raw const v).cast::<u8>(), core::mem::size_of::<T>())
    };
    write_bytes(ptr, bytes)
}

/// Read a NUL-terminated string of at most `max` bytes (excluding the NUL).
///
/// `None` for a null pointer, a bad range, a fault, or no NUL within `max` —
/// the last on purpose: a truncated path names a *different file*, so the
/// callers that bounded at 256 treat over-length as a rejection, not a cut.
///
/// Reads up to the end of the current page at a time, so a string that ends
/// just before an unmapped page is read successfully rather than failed by a
/// speculative over-read — the same reason Linux's `strncpy_from_user` stops
/// at page boundaries.
#[must_use]
pub fn read_cstr(ptr: u64, max: usize) -> Option<Vec<u8>> {
    if ptr == 0 {
        return None;
    }
    let mut out = Vec::new();
    let mut at = ptr;
    let mut buf = [0u8; 256];
    while out.len() < max {
        let to_page_end = 4096 - (at & 0xfff) as usize;
        let want = (max - out.len()).min(to_page_end).min(buf.len());
        if !read_bytes(at, &mut buf[..want]) {
            return None;
        }
        if let Some(nul) = buf[..want].iter().position(|&b| b == 0) {
            out.extend_from_slice(&buf[..nul]);
            return Some(out);
        }
        out.extend_from_slice(&buf[..want]);
        at += want as u64;
    }
    None
}

/// Prove `CR0.WP` is enforcing, and that a kernel write to a shared page breaks
/// the sharing rather than corrupting the peer.
///
/// # What this is a regression test for
///
/// Until 2026-09-07 this kernel ran with `CR0.WP` clear, so a supervisor write
/// ignored the `R/W` bit entirely. `copy_to_user` writes through the *user*
/// virtual address, so after a `fork` a `read(2)` into a buffer the child had
/// not yet written landed **in the frame the parent was still mapping**: no
/// fault, no copy-on-write break, no error, and the parent's memory changed
/// underneath it. The second check below is that exact scenario, built by hand:
/// two user VAs onto one CoW-marked frame with a share count of 2, a kernel
/// write through one of them, and then the question the bug got wrong — *did
/// the other VA see it?*
///
/// The first check is the other half. A user page that is read-only **on
/// purpose** (`PteProt::USER_RX`, an ELF text segment, an `mprotect(PROT_READ)`
/// range) must still refuse a kernel write: `akuma_cow` answers `Fault` for an
/// unmarked read-only page, the fault falls through to the user-copy fixup, and
/// [`write_bytes`] reports failure. Without that half, turning `WP` on would
/// simply move the corruption rather than fix it.
fn write_protect_check(t: &mut Suite) {
    use crate::paging::{self, MemAttr, PteProt};
    use crate::phys::phys_ptr;
    const RO_VA: u64 = 0x14_0000_0000;
    const SHARED_A: u64 = 0x15_0000_0000;
    const SHARED_B: u64 = 0x15_0000_1000;

    t.check("wp: CR0.WP is set", write_protect_on());

    // 1. A user page that is read-only on purpose refuses a kernel write.
    let Some(ro_pa) = akuma_pmm::alloc_page() else {
        t.check("wp: frame for the read-only page", false);
        return;
    };
    // SAFETY: a fresh PMM frame, reached through the physmap (supervisor).
    unsafe { core::ptr::write_bytes(phys_ptr::<u8>(ro_pa as u64), 0xA5, 4096) };
    if paging::map_page(RO_VA as usize, ro_pa as u64, PteProt::USER_RX, MemAttr::WriteBack) {
        let wrote = write_bytes(RO_VA, b"must not land");
        // SAFETY: the same frame, through the physmap.
        let untouched = unsafe { phys_ptr::<u8>(ro_pa as u64).read_volatile() == 0xA5 };
        t.check("wp: a kernel write to a read-only user page is refused", !wrote);
        t.check("wp: and the page is unchanged", untouched);
        t.check("wp: AC is clear after the refused write", !ac_set());
        if let Some(pa) = paging::unmap_page(RO_VA as usize) {
            akuma_pmm::free_page(pa as usize, 0);
        }
    } else {
        akuma_pmm::free_page(ro_pa, 0);
        t.check("wp: map the read-only page", false);
    }

    // 2. Two VAs, one CoW-marked frame, share count 2 — a forked pair, by hand.
    let Some(shared_pa) = akuma_pmm::alloc_page() else {
        t.check("wp: frame for the shared page", false);
        return;
    };
    // SAFETY: a fresh PMM frame, through the physmap.
    unsafe { core::ptr::write_bytes(phys_ptr::<u8>(shared_pa as u64), 0x5A, 4096) };
    // A CoW-demoted page: read-only in the hardware *and* marked, which is what
    // `PteProt::USER_RW.cow()` used to produce. The marker is not a permission
    // and so is not part of `PteProt` — it is `map_page_pte`'s fourth argument,
    // and the demotion (`USER_RW` -> `USER_RO`) is stated here rather than
    // hidden in a constructor. A pair that stayed writable would never fault and
    // this test would pass while proving nothing.
    //
    // Mapped through a **borrowed view** of the kernel's own root, the same one
    // `idt::faulting_address_space` builds — so the pages this test installs and
    // the walk the fault handler does are one implementation. `new_shared`'s
    // ledger owns nothing, so the view frees nothing when it drops; the unmap
    // and the `cow_ref_dec` at the end of this function are still by hand.
    let Some(mut kroot) = akuma_mmu::UserAddressSpace::new_shared(paging::active_root() as usize)
    else {
        t.check("wp: borrowed view of the kernel root", false);
        akuma_pmm::free_page(shared_pa, 0);
        return;
    };
    let mapped = kroot.map_page_pte(SHARED_A as usize, shared_pa, PteProt::USER_RO, true)
        && kroot.map_page_pte(SHARED_B as usize, shared_pa, PteProt::USER_RO, true);
    if !mapped {
        t.check("wp: map the shared pair", false);
        akuma_pmm::free_page(shared_pa, 0);
        return;
    }
    // Two address spaces hold it. `cow_ref_inc` twice is what a `fork` leaves
    // behind, and it is what makes `akuma_cow` answer `Copy` rather than
    // `TakeInPlace` — the branch that has to allocate.
    akuma_pmm::cow_ref_inc(shared_pa);
    akuma_pmm::cow_ref_inc(shared_pa);

    let wrote = write_bytes(SHARED_A, b"private");
    t.check("wp: a kernel write to a CoW page succeeds", wrote);

    // The write must be visible through the VA that was written...
    let mut back = [0u8; 7];
    let read_ok = read_bytes(SHARED_A, &mut back);
    t.check("wp: and is visible through that mapping", read_ok && &back == b"private");

    // ...and INVISIBLE through the peer. This is the whole bug: with `WP`
    // clear both VAs saw it, because there was one frame and no break.
    let mut peer = [0u8; 7];
    let peer_ok = read_bytes(SHARED_B, &mut peer);
    t.check("wp: and NOT through the peer mapping (the sharing broke)", peer_ok && peer == [0x5A; 7]);

    // The two VAs must now be different frames, which is what "broke" means.
    let pa_a = paging::translate(SHARED_A as usize).unwrap_or(0);
    let pa_b = paging::translate(SHARED_B as usize).unwrap_or(0);
    t.check("wp: the pair no longer shares a frame", pa_a != pa_b && pa_a != 0 && pa_b != 0);

    for va in [SHARED_A, SHARED_B] {
        if let Some(pa) = paging::unmap_page(va as usize)
            && akuma_pmm::cow_ref_dec(pa as usize)
        {
            akuma_pmm::free_page(pa as usize, 0);
        }
    }
}

/// Prove SMAP is enforcing, not just enabled.
///
/// Maps one **user-accessible** page (`PteProt::USER_RW`), then:
///
/// 1. `CR4.SMAP`/`SMEP` are on exactly when CPUID advertises them.
/// 2. The raw copy primitive — no `stac` — reading that page returns `EFAULT`
///    when SMAP is on: the CPU refused a supervisor access to a user page, the
///    `#PF` landed inside `rep movsb`, and the fixup turned it into an error.
///    That is the whole point of SMAP, observed — and since 2026-09-07 it is
///    observed by turning the *brackets* off at
///    `akuma_user_access::set_smap_active` rather than by relying on the shared
///    copy loop having none, which was itself the bug. (Without SMAP the probe
///    is skipped and says so rather than passing vacuously.)
/// 3. [`read_bytes`]/[`write_bytes`] — bracketed — read and write the page
///    correctly, and leave `AC` clear afterwards.
/// 4. A bracketed copy that *faults* (unmapped source) also leaves `AC` clear:
///    the fixup path returns through `clac`, not around it.
pub fn smoke_test(t: &mut Suite, st: SmapStatus) {
    use crate::paging::{self, MemAttr, PteProt};
    use crate::phys::phys_ptr;
    const USER_PAGE_VA: u64 = 0x12_0000_0000;
    const UNMAPPED_VA: u64 = 0x13_0000_0000;

    let (smap_on, smep_on) = cr4_bits();
    t.check_eq("smap: CR4.SMAP follows CPUID", u64::from(smap_on), u64::from(st.cpuid_smap));
    t.check_eq("smap: CR4.SMEP follows CPUID", u64::from(smep_on), u64::from(st.cpuid_smep));

    let free_before = akuma_pmm::free_count();
    let Some(pa) = akuma_pmm::alloc_page() else {
        t.check("smap: frame for the user page", false);
        return;
    };
    // SAFETY: a fresh PMM frame, reached through the physmap (supervisor).
    unsafe {
        let p = phys_ptr::<u8>(pa as u64);
        for i in 0..4096 {
            p.add(i).write_volatile((i as u8) ^ 0x3C);
        }
    }
    if !paging::map_page(USER_PAGE_VA as usize, pa as u64, PteProt::USER_RW, MemAttr::WriteBack) {
        akuma_pmm::free_page(pa, 0);
        t.check("smap: map the user page", false);
        return;
    }

    let mut dst = [0u8; 64];

    // The **shared** copy loop (`akuma-user-access`), which is what every
    // syscall served by `akuma-syscalls-glue` copies through.
    //
    // This block asserted the opposite until 2026-09-07: it read
    // `copy_from_user_safe` as "the unbracketed copy" and checked that SMAP
    // refused it. That was true and it was a *bug* — the shared crate was the
    // tree's only x86 user copy without `stac`/`clac`, while
    // `read_bytes`/`write_bytes` two checks below have had them since SMAP was
    // turned on. It survived because nothing on this target called it with a
    // real ring-3 pointer: the boot suite runs inside `BypassValidationGuard`
    // and hands it kernel-stack buffers. Folding the first syscall into glue
    // is what found it — as a hang, not an error
    // (`docs/archive/AKUMA_AMD64_C1_STEP3_PREREQUISITES.md`).
    //
    // So: it must now succeed and be byte-exact, like the local pair.
    // SAFETY: a mapped, user-accessible page; a fault would be recovered.
    let raw = unsafe { copy_from_user_safe(dst.as_mut_ptr(), USER_PAGE_VA as *const u8, dst.len()) };
    let exact = dst.iter().enumerate().all(|(i, &b)| b == (i as u8) ^ 0x3C);
    t.check("smap: the shared copy loop reads a user page (it brackets itself)", raw.is_ok() && exact);
    t.check("smap: AC is clear after the shared copy", !ac_set());

    // And SMAP's own behaviour, still observed rather than assumed — by turning
    // the bracketing *off* at the flag and watching the same copy be refused.
    // That tests two things the old check could not: that an undeclared ring-0
    // access to a user page really is refused, and that
    // `akuma_user_access::set_smap_active` is actually what drives the
    // brackets. Restored immediately; nothing else runs in between.
    if smap_on {
        akuma_user_access::set_smap_active(false);
        dst.fill(0);
        // SAFETY: as above, and now deliberately undeclared — the fault is
        // recovered by the copy loop's fixup, which is the point.
        let unbracketed =
            unsafe { copy_from_user_safe(dst.as_mut_ptr(), USER_PAGE_VA as *const u8, dst.len()) };
        akuma_user_access::set_smap_active(true);
        t.check_eq(
            "smap: with the brackets off, the same read is refused (EFAULT)",
            unbracketed.err().unwrap_or(0),
            14,
        );
        t.check("smap: AC is clear after the refused read", !ac_set());
    } else {
        t.note("smap: (CPUID lacks SMAP) bracket-off probe skipped", 0);
    }

    dst.fill(0);
    let ok = read_bytes(USER_PAGE_VA, &mut dst);
    let exact = dst.iter().enumerate().all(|(i, &b)| b == (i as u8) ^ 0x3C);
    t.check("smap: a bracketed read of a user page succeeds and is byte-exact", ok && exact);
    t.check("smap: AC is clear after the read", !ac_set());

    let ok = write_bytes(USER_PAGE_VA + 100, b"stac/clac");
    // SAFETY: the same frame, through the physmap.
    let landed = unsafe {
        let p = phys_ptr::<u8>(pa as u64).add(100);
        (0..9).all(|i| p.add(i).read_volatile() == b"stac/clac"[i])
    };
    t.check("smap: a bracketed write to a user page lands", ok && landed);
    t.check("smap: AC is clear after the write", !ac_set());

    let ok = read_bytes(UNMAPPED_VA, &mut dst);
    t.check("smap: a bracketed copy that faults returns false and leaves AC clear", !ok && !ac_set());

    if let Some(pa) = paging::unmap_page(USER_PAGE_VA as usize) {
        akuma_pmm::free_page(pa as usize, 0);
    }
    // One page directory and one page table for a new 1 GiB region, kept by
    // `unmap_page` as `idt::smoke_test` explains.
    t.check_eq(
        "smap: only the two intermediate tables retained",
        (free_before - akuma_pmm::free_count()) as u64,
        2,
    );

    // **After** the accounting check, not before. The `WP` checks map three more
    // 1 GiB regions and `unmap_page` retains each one's directory and table, so
    // running them first turns this into a five-frame "leak" that is nothing of
    // the sort. Its own frame accounting is done per-page inside it instead.
    write_protect_check(t);
}

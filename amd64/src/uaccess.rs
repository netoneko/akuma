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
/// `RFLAGS.AC`.
const RFLAGS_AC: u64 = 1 << 18;

/// Turn on `CR4.SMAP` and `CR4.SMEP` where CPUID advertises them.
///
/// Call once, early — before the first syscall and before the self-tests, so
/// everything after runs under the enforcement it will ship with. Safe to call
/// with paging up: the bits change how *future* accesses are checked and
/// nothing in this kernel touches a user page without [`read_bytes`] /
/// [`write_bytes`] (the loader and the fork/exec page copies go through the
/// physmap, which is supervisor-only).
pub fn init_smap() -> SmapStatus {
    let ebx: u32;
    // SAFETY: `cpuid` is unprivileged and side-effect-free. `rbx` is
    // callee-saved and LLVM reserves it, so it is saved and restored by hand
    // rather than named as a clobber — naming it is a compile error (same
    // shape as `net::has_rdrand`). Leaf 7 needs `ecx` = 0 (subleaf).
    unsafe {
        core::arch::asm!(
            "mov {tmp:r}, rbx",
            "cpuid",
            "mov {ebx:e}, ebx",
            "mov rbx, {tmp:r}",
            tmp = out(reg) _,
            ebx = out(reg) ebx,
            inout("eax") 7u32 => _,
            inout("ecx") 0u32 => _,
            out("edx") _,
            options(nomem, nostack, preserves_flags),
        );
    }
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
    }
    SmapStatus { cpuid_smap, cpuid_smep }
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

/// Prove SMAP is enforcing, not just enabled.
///
/// Maps one **user-accessible** page (`Prot::USER_RW`), then:
///
/// 1. `CR4.SMAP`/`SMEP` are on exactly when CPUID advertises them.
/// 2. The raw copy primitive — no `stac` — reading that page returns `EFAULT`
///    when SMAP is on: the CPU refused a supervisor access to a user page, the
///    `#PF` landed inside `rep movsb`, and the fixup turned it into an error.
///    That is the whole point of SMAP, observed. (Without SMAP the same read
///    succeeds, and the check says so rather than passing vacuously.)
/// 3. [`read_bytes`]/[`write_bytes`] — bracketed — read and write the page
///    correctly, and leave `AC` clear afterwards.
/// 4. A bracketed copy that *faults* (unmapped source) also leaves `AC` clear:
///    the fixup path returns through `clac`, not around it.
pub fn smoke_test(t: &mut Suite, st: SmapStatus) {
    use crate::paging::{self, MemAttr, Prot};
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
    if !paging::map_page(USER_PAGE_VA as usize, pa as u64, Prot::USER_RW, MemAttr::WriteBack) {
        akuma_pmm::free_page(pa, 0);
        t.check("smap: map the user page", false);
        return;
    }

    let mut dst = [0u8; 64];
    // SAFETY: the source is a mapped USER page read from ring 0 WITHOUT `stac`
    // — with SMAP on this must fault, and the fault is recovered.
    let raw = unsafe { copy_from_user_safe(dst.as_mut_ptr(), USER_PAGE_VA as *const u8, dst.len()) };
    if smap_on {
        t.check_eq(
            "smap: an unbracketed kernel read of a user page is refused (EFAULT)",
            raw.err().unwrap_or(0),
            14,
        );
    } else {
        t.check("smap: (CPUID lacks SMAP) an unbracketed kernel read of a user page succeeds", raw.is_ok());
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
}

//! User memory access: the range check, the prefault, and the copy — one helper.
//!
//! # The two halves, and why they used to be two
//!
//! Until 2026-08-14 this module held only the *copy*
//! ([`copy_from_user_safe`]/[`copy_to_user_safe`]): an assembly byte loop plus a
//! recovery trampoline registered as the thread's user-copy fault handler. If a
//! `ldrb`/`strb` in that loop touches an unmapped page, the EL1 abort handler
//! (`src/exceptions.rs`, `EC=0x25` with `ELR` in kernel code) rewrites `ELR_EL1`
//! to the trampoline, which returns `EFAULT`. That is all "safe" ever meant here:
//! **an unmapped user address cannot panic the kernel.** The copy checked nothing.
//!
//! The *check* lived in the bin crate as `syscall::validate_user_ptr`, and it was
//! opt-in: 126 call sites against 167 copies. Two consequences, both real:
//!
//! 1. The recovery net only catches **unmapped** addresses. A *mapped kernel* VA
//!    passed as a destination is written by the byte loop with no fault, no
//!    `EFAULT` and no diagnostic. `validate_user_ptr` deliberately does not
//!    exclude the kernel identity-mapped range (see [`user_range_ok`]), so the
//!    range check was the only thing standing there — and it could be skipped.
//! 2. The raw `(dst, src, len)` signature put the *kernel*-side length invariant
//!    on the caller. The trampoline protects the user pointer; nothing protects
//!    the kernel buffer, so a `len` larger than the kernel array is an ordinary
//!    mapped over-read or over-write — no fault, no diagnostic.
//!
//! So the two are folded: [`copy_to_user`], [`copy_from_user`],
//! [`write_user_val`] and [`read_user_into`] validate, prefault and copy, take
//! slices (so `len` cannot disagree with the buffer) and are **safe `fn`s**.
//! `docs/archive/UNSAFE_AUDIT.md` §4 P0, and
//! `docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md` Phase 5.
//!
//! # Prefaulting is a parameter, not a default
//!
//! Validation's second job is to make the range *present*, by demand-paging lazy
//! pages ([`prefault_user_range`]) so the copy cannot fault at all. That
//! allocates frames, takes the address space's `as_lock` and reads files — so it
//! must not run from a context that already holds a lock the reclaim path takes,
//! or from the abort path itself. Syscall arms want [`Prefault::Yes`]; fault-time
//! and non-user-memory callers pass [`Prefault::No`] and say why.

use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, Ordering};
use crate::threading::set_user_copy_fault_handler;

unsafe extern "C" {
    fn __arch_copy_user_memory(dst: *mut u8, src: *const u8, len: usize) -> u64;
    fn __arch_copy_user_memory_bytes(dst: *mut u8, src: *const u8, len: usize) -> u64;
    fn __arch_copy_user_fault();
}

global_asm!(
    r#"
.text
.global __arch_copy_user_memory
.global __arch_copy_user_fault

// x0 = dst, x1 = src, x2 = len
// Returns 0 on success, non-zero (EFAULT) on error
//
// Widest-first: 64, then 16, then 8, then the byte tail. This used to be a
// single byte-at-a-time loop, which cost ~16x an in-kernel memcpy per byte and
// was the dominant term in a warm read(2) — measurements and the reasoning are
// in docs/archive/USER_COPY_BYTE_LOOP.md.
//
// THREE INVARIANTS, all load-bearing:
//
//   1. LEAF AND STACKLESS. `__arch_copy_user_fault` returns through x30, which
//      only works because nothing here writes x30 or sp. On a fault the
//      exception handler rewrites ELR_EL1 to that trampoline, and its `ret`
//      must land back in the Rust caller. Give this function a prologue and a
//      mid-copy fault returns to garbage. Do not add one.
//   2. CALLER-SAVED REGISTERS ONLY (x3-x10 here), because the trampoline restores
//      nothing before returning EFAULT. Note what this does NOT license: AAPCS64
//      says x0-x17 are the *caller's* to lose, but an exception is not a call, and
//      invariant 3 is the half that bites.
//   3. THE EXCEPTION MUST NOT EAT x3-x10. A store here can fault (the first
//      touch of a lazy destination page), and `try_resolve_el1_user_copy_lazy_fault`
//      resolves it and ERETs back to RE-EXECUTE that store — so every register
//      the store reads has to survive `sync_el1_handler`. That vector saved
//      x0-x3/x29/x30 only, which was invisible while this was a byte loop living
//      in x3 and became silent read(2) corruption the moment it widened into
//      x3-x10 (docs/archive/BUSYBOX_HASH_MISCOMPUTE.md). Widening this loop
//      further, or moving it to other registers, is safe only as long as the
//      vector saves them: `test_el1_sync_exception_preserves_gprs` is the guard.
//   4. UNALIGNED IS ALLOWED. SCTLR_EL1.A is 0 (src/boot.rs forces SA/SA0 off
//      and leaves A at its reset value, which is clear on both QEMU virt and
//      Firecracker/KVM), so unaligned ldp/stp to Normal memory is fine. Device
//      memory would fault on multi-register access regardless of A; no user VA
//      is Device-mapped.
//
// A fault mid-chunk can leave the destination partly written. That was already
// true byte-wise, and the contract is a bare EFAULT rather than Linux's
// bytes-not-copied, so the granularity is not observable.
__arch_copy_user_memory:
    cbz     x2, 9f

    cmp     x2, #64
    b.lo    2f
1:  // 64 bytes per iteration
    ldp     x3, x4, [x1, #0]
    ldp     x5, x6, [x1, #16]
    ldp     x7, x8, [x1, #32]
    ldp     x9, x10, [x1, #48]
    stp     x3, x4, [x0, #0]
    stp     x5, x6, [x0, #16]
    stp     x7, x8, [x0, #32]
    stp     x9, x10, [x0, #48]
    add     x1, x1, #64
    add     x0, x0, #64
    sub     x2, x2, #64
    cmp     x2, #64
    b.hs    1b

2:  cmp     x2, #16
    b.lo    4f
3:  // 16 bytes per iteration
    ldp     x3, x4, [x1], #16
    stp     x3, x4, [x0], #16
    sub     x2, x2, #16
    cmp     x2, #16
    b.hs    3b

4:  cmp     x2, #8
    b.lo    6f
    // one 8-byte chunk; x2 < 16 here, so this cannot loop
    ldr     x3, [x1], #8
    str     x3, [x0], #8
    sub     x2, x2, #8

6:  cbz     x2, 9f
7:  // byte tail, at most 7
    ldrb    w3, [x1], #1
    strb    w3, [x0], #1
    subs    x2, x2, #1
    b.ne    7b

9:  mov     x0, #0
    ret

// Byte-at-a-time variant: the pre-2026-08-27 loop, kept as the reference oracle
// for `copy_loop_differential_sweep` — the widened loop above is only trustworthy
// against an implementation too simple to be wrong. Same invariants as above,
// leaf and stackless, because the same fault trampoline returns through x30 for
// both. Not reachable from any copy path.
.global __arch_copy_user_memory_bytes
__arch_copy_user_memory_bytes:
    cbz     x2, 19f
18: ldrb    w3, [x1], #1
    strb    w3, [x0], #1
    subs    x2, x2, #1
    b.ne    18b
19: mov     x0, #0
    ret

// Fault handler - jumped to by exception handler
// Returns EFAULT (14)
__arch_copy_user_fault:
    mov x0, #14
    ret
    "#
);

/// `EFAULT`, as the byte loop's trampoline returns it and as the syscall ABI wants
/// it (`x0 = -errno` happens at the syscall boundary, not here).
const EFAULT: u64 = akuma_primitives::errno::EFAULT as u64;

/// The 48-bit user VA limit — the only necessary upper bound, because the kernel's
/// own addresses live in the TTBR1 half (bit 63 set).
///
/// Not a smaller cap, and that is load-bearing: Go on AArch64 allocates goroutine
/// stacks and M-structs from high arenas (e.g. `0x203e000000` ≈ 130 GB) via `mmap`,
/// so any fixed small cap — 4 GB, `stack_top`, … — rejects valid user pointers.
pub const USER_VA_LIMIT: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Let a kernel address stand in for a user VA, for the boot self-tests that drive
/// `handle_syscall` directly with kernel-stack buffers.
///
/// Lives here rather than in the bin crate's syscall module because the check it
/// disables lives here now; `syscall::BYPASS_VALIDATION` is a re-export, so the ~85
/// boot-test call sites are unchanged.
///
/// # Per thread, not per kernel (2026-08-14)
///
/// This was a single kernel-wide `AtomicBool`, flipped by ~85 hand-paired
/// `store(true)`/`store(false)` sites — so while any one thread had it on,
/// validation was off for **every thread on every core**. `USER_COPY_FOLD.md` §11
/// item 3 recorded that as a defect with "no live leak path today", because a
/// kernel VA passed validation anyway and turning the bypass off early cost nothing.
///
/// The AP-bit fix (§7 there) made it live, and `test_futex_wake_one_of_two` caught
/// it within one boot. That test is the only futex test with **two** syscalls inside
/// one bypass window: `FUTEX_WAKE(1)`, then `FUTEX_WAKE(INT_MAX)`. The waiter woken
/// by the first runs in between and ends with its own `store(false)` — which, on a
/// global flag, closed the *main thread's* window. Its second wake then validated a
/// kernel `.bss` address for real, got `EFAULT`, and the second waiter was never
/// woken. Under the old presence-only check the same race happened and was simply
/// invisible.
///
/// Making the flag per-thread fixes that by construction and needs no call-site
/// changes: `store`/`load` keep the `AtomicBool` signatures and address the calling
/// thread's slot. **Still open** from that item: the pairs are hand-written, so an
/// abnormal exit between a `store(true)` and its `store(false)` leaves that thread's
/// slot on — an RAII guard is what closes it.
pub struct BypassValidation {
    per_thread: [AtomicBool; akuma_primitives::preempt::MAX_THREADS],
}

impl BypassValidation {
    const fn new() -> Self {
        Self { per_thread: [const { AtomicBool::new(false) }; akuma_primitives::preempt::MAX_THREADS] }
    }

    /// Turn the bypass on or off **for the calling thread**.
    ///
    /// An out-of-range tid is ignored rather than panicking, matching every other
    /// per-thread table in the tree (`DroppedWindowLedger`, the thread-state arrays).
    #[inline]
    pub fn store(&self, on: bool, order: Ordering) {
        if let Some(slot) = self.per_thread.get(akuma_primitives::preempt::current_tid()) {
            slot.store(on, order);
        }
    }

    /// Whether the calling thread is inside a bypass window. An out-of-range tid
    /// reads as `false` — validation on, the safe answer.
    #[inline]
    #[must_use]
    pub fn load(&self, order: Ordering) -> bool {
        self.per_thread
            .get(akuma_primitives::preempt::current_tid())
            .is_some_and(|slot| slot.load(order))
    }
}

pub static BYPASS_VALIDATION: BypassValidation = BypassValidation::new();

/// RAII form of [`BYPASS_VALIDATION`]: on for the calling thread until dropped, and
/// **restoring what it found** rather than forcing `false`, so windows nest.
///
/// The other half of `USER_COPY_FOLD.md` §11 item 3. Nothing enforces the pairing of
/// the ~85 raw `store(true)`/`store(false)` sites, so a `?`, an early `return`, or a
/// panic between a pair leaves that thread's bypass on for whatever runs in the
/// thread next. Prefer this at any new site.
#[must_use]
pub struct BypassValidationGuard {
    prev: bool,
}

impl BypassValidationGuard {
    /// Bypass user-pointer validation on this thread until the guard drops.
    pub fn new() -> Self {
        let prev = BYPASS_VALIDATION.load(Ordering::Acquire);
        BYPASS_VALIDATION.store(true, Ordering::Release);
        Self { prev }
    }
}

impl Default for BypassValidationGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BypassValidationGuard {
    fn drop(&mut self) {
        BYPASS_VALIDATION.store(self.prev, Ordering::Release);
    }
}

/// Whether a validated range should have its lazy pages faulted in before the copy.
///
/// [`Prefault::Yes`] is what a syscall arm wants: [`prefault_user_range`] allocates
/// frames, takes the address space's `as_lock` and may read from a file, all of
/// which is fine on a syscall stack and fatal on a fault-handling one. Callers that
/// pass [`Prefault::No`] should say why at the call site.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prefault {
    /// Demand-page lazy pages covering the range (syscall arms).
    Yes,
    /// Check the range only. For fault-time callers, and for "user" pointers that
    /// are really something else (a rump client buffer).
    No,
}

/// The side-effect-free half of the range check: is `[ptr, ptr+len)` a plausible
/// user range at all?
///
/// Rejects the null page, an overflowing `ptr + len`, and anything reaching past
/// [`USER_VA_LIMIT`]. It does **not** exclude the kernel identity-mapped range
/// (`0x4000_0000..0x6000_0000`) — that exclusion was tried and reverted, because
/// Bun's JSC `mmap`s at `0x5000_0000` and every such pointer is legitimate. What
/// stands in for it is the page-table walk in [`validate_user_range`] plus the EL1
/// data-abort recovery, and the reason the copy helpers now fold the check in is
/// that the recovery only catches *unmapped* addresses (see the module header).
#[must_use]
pub const fn user_range_ok(ptr: u64, len: usize) -> bool {
    if ptr < 0x1000 {
        return false;
    }
    match ptr.checked_add(len as u64) {
        Some(end) => end <= USER_VA_LIMIT,
        None => false,
    }
}

/// Full validation: [`user_range_ok`], then confirm the range is mapped — faulting
/// lazy pages in when asked.
///
/// This is what the bin crate's `syscall::validate_user_ptr` was; it moved here so
/// the copy helpers can call it, which is what makes the check unskippable.
#[must_use]
pub fn validate_user_range(ptr: u64, len: usize, prefault: Prefault) -> bool {
    if BYPASS_VALIDATION.load(Ordering::Acquire) {
        return true;
    }
    if !user_range_ok(ptr, len) {
        return false;
    }
    if crate::mmu::is_current_user_range_mapped(ptr as usize, len) {
        return true;
    }
    match prefault {
        // Re-assert the real predicate after the fill. `prefault_user_range` skips
        // pages that are already *present* — deliberately, so it never re-maps a
        // `PROT_NONE` guard or a lazily-filled page twice — so on its own it would
        // report success for a range that is mapped but not EL0-accessible, which is
        // exactly the case the AP test in `is_current_user_range_mapped` exists to
        // reject. The second walk only runs on a path that just did per-page frame
        // allocation and possibly file I/O.
        Prefault::Yes => {
            prefault_user_range(ptr as usize, len)
                && crate::mmu::is_current_user_range_mapped(ptr as usize, len)
        }
        Prefault::No => false,
    }
}

/// Demand-page any lazy user pages covering `[start, start+len)` so a kernel-side
/// access can touch them.
///
/// **Phase 7f pre-flight**: this runs inside BKL-free syscall windows (every
/// whole-fn net/vfs guard reaches it through [`validate_user_range`], and the
/// opt-out list made `getrandom`'s prologue join that class), so the BKL no longer
/// serializes its PTE installs against a peer core. Each page's PTE install +
/// frame bookkeeping therefore runs under the address space's `as_lock`, the same
/// fix Phase 7e applied to the three signal-path PTE sites. Frame allocation and
/// the file fill stay OUTSIDE that hold: the hold masks IRQs and `as_lock` is a
/// non-reentrant `Spinlock`, so block I/O must never run under it, and the PMM's
/// pressure path (`reclaim_clean_file_pages`) re-enters `as_lock` per page.
#[must_use]
pub fn prefault_user_range(start: usize, len: usize) -> bool {
    let page_start = start & !0xFFF;
    let page_end = (start + len + 0xFFF) & !0xFFF;
    // The `as_lock` guarding THIS address space's page tables lives on the
    // thread-group leader that owns the L0 root — a CLONE_VM sibling gets a fresh
    // `as_lock` from `fork_process`, so holding the current thread's would exclude
    // nothing (`process/bkl_guard.rs`'s rule 1). Resolved once for the whole range;
    // it cannot change under us, this is our own address space.
    let as_lock_owner = crate::process::address_space_owner_pid_for_fault()
        .and_then(crate::process::lookup_process_shared);
    let mut va = page_start;
    while va < page_end {
        if !crate::mmu::is_current_user_page_mapped(va) {
            let Some((flags, source, _region_start, _region_size)) =
                crate::process::lazy_region_lookup(va)
            else {
                return false;
            };
            let map_flags = match &source {
                crate::process::LazySource::File { .. } => {
                    if flags == 0 { crate::mmu::user_flags::RW_NO_EXEC } else { flags }
                }
                _ => crate::mmu::user_flags::RW_NO_EXEC,
            };
            let Some(page_addr) = akuma_pmm::alloc_page_zeroed() else {
                return false;
            };
            let page_frame = crate::PhysFrame::new(page_addr);
            if let crate::process::LazySource::File {
                ref path, inode, file_offset, filesz, segment_va, ..
            } = source
            {
                let pg_data_start = core::cmp::max(va, segment_va);
                let pg_data_end = core::cmp::min(va + 0x1000, segment_va + filesz);
                if pg_data_start < pg_data_end {
                    let dst_off = pg_data_start - va;
                    let file_off = file_offset + (pg_data_start - segment_va);
                    let read_len = pg_data_end - pg_data_start;
                    let page_ptr = crate::mmu::phys_to_virt(page_frame.addr);
                    // SAFETY: `page_ptr` is the kernel mapping of a page just
                    // allocated here and not yet published to any page table, so
                    // this is the only reference to it; `dst_off + read_len` is
                    // bounded by the 4 KiB page by the two clamps above.
                    let page_buf = unsafe {
                        core::slice::from_raw_parts_mut(
                            page_ptr.cast::<u8>().add(dst_off),
                            read_len,
                        )
                    };
                    let rt = crate::runtime::runtime();
                    let got = if inode == 0 {
                        (rt.read_at)(path, file_off, page_buf)
                    } else {
                        (rt.read_at_by_inode)(path, inode, file_off, page_buf)
                    };
                    // The range is already clamped to `filesz`, so anything less
                    // than `read_len` is a defect, not EOF — same contract as
                    // the demand-fault fill's `[FILL-SHORT]` in
                    // `src/exceptions.rs`. But this site is the one that
                    // instrument could never see: the page installed below is
                    // PRESENT, so no later fault re-fills it, and the zero tail
                    // of `alloc_page_zeroed`'s frame reads back as file data.
                    // The result used to be dropped (`let _ =`), which made the
                    // defect silent; count and name it instead.
                    if got != Ok(read_len) {
                        akuma_pmm::DP_PREFAULT_FILL_SHORT.fetch_add(1, Ordering::Relaxed);
                        crate::safe_print!(224,
                            "[FILL-SHORT/prefault] pid={} inode={} file_off={:#x} want={} got={:?} va={:#x} — page installed zero-filled\n",
                            crate::process::read_current_pid().unwrap_or(0), inode, file_off, read_len, got, va);
                    }
                }
            }
            // Everything above (alloc + file fill) ran with no `as_lock` held.
            // The PTE install and the frame bookkeeping must be atomic against
            // a peer core's page-table edit on this same page — in particular
            // against `reclaim_clean_file_pages`, whose `try_evict_ro_page`
            // clears a live RO PTE and only returns the frame if it is already
            // tracked. Installing outside the hold lets it observe the
            // mapped-but-untracked instant: it clears our PTE, declines to free
            // (untracked), and we then track a frame that is no longer mapped —
            // a re-fault leak. One hold per page, never spanning the loop.
            let owner_pid = crate::process::read_current_pid().unwrap_or(0);
            let owner = crate::process::lookup_process_shared(owner_pid);
            let install = || {
                // SAFETY: installing a mapping for our own address space at a VA
                // this loop has just confirmed unmapped, with a frame nothing else
                // references yet.
                let (table_frames, installed) =
                    unsafe { crate::mmu::map_user_page(va, page_frame.addr, map_flags) };
                if let Some(owner) = owner {
                    if installed {
                        owner.address_space.track_user_frame(page_frame);
                    }
                    for tf in &table_frames {
                        owner.address_space.track_page_table_frame(*tf);
                    }
                }
                (table_frames, installed)
            };
            let (table_frames, installed) = match as_lock_owner {
                Some(leader) => leader.with_as_locked(install),
                None => install(),
            };
            // Frees run after the hold is released. Ownership rule, matching
            // `exceptions.rs`'s `ensure_user_page_mapped` sibling: the data
            // frame goes back to the PMM iff the PTE CAS race was lost (nothing
            // mapped it) or there is no owner to track it. A previous version
            // freed it iff `installed && owner.is_none()`, which leaked the
            // frame on every lost CAS race with a live owner.
            let tid = akuma_primitives::preempt::current_tid() as u32;
            if !installed || owner.is_none() {
                akuma_pmm::free_page(page_frame.addr, tid);
            }
            if owner.is_none() {
                for tf in table_frames {
                    akuma_pmm::free_page(tf.addr, tid);
                }
            }
        }
        va += 4096;
    }
    true
}

// ============================================================================
// The folded API: validate, prefault, copy. No `unsafe` at any call site.
// ============================================================================

/// Copy `src` out to user memory at `dst_user`.
///
/// The length comes from the slice, so it cannot disagree with the kernel-side
/// buffer — the invariant the raw `(dst, src, len)` form left to the caller.
pub fn copy_to_user(dst_user: u64, src: &[u8]) -> Result<(), u64> {
    copy_to_user_with(dst_user, src, Prefault::Yes)
}

/// [`copy_to_user`] with an explicit prefault choice. See [`Prefault`].
pub fn copy_to_user_with(dst_user: u64, src: &[u8], prefault: Prefault) -> Result<(), u64> {
    if src.is_empty() {
        return Ok(());
    }
    if !validate_user_range(dst_user, src.len(), prefault) {
        return Err(EFAULT);
    }
    // SAFETY: `dst_user` is validated for `src.len()` bytes above (or explicitly
    // bypassed by a boot test); the kernel side is a slice, so its length is its
    // own. A fault on the user side lands in the recovery trampoline.
    unsafe { copy_to_user_safe(dst_user as *mut u8, src.as_ptr(), src.len()) }
}

/// Copy from user memory at `src_user` into `dst`, filling it exactly.
pub fn copy_from_user(dst: &mut [u8], src_user: u64) -> Result<(), u64> {
    copy_from_user_with(dst, src_user, Prefault::Yes)
}

/// [`copy_from_user`] with an explicit prefault choice. See [`Prefault`].
pub fn copy_from_user_with(dst: &mut [u8], src_user: u64, prefault: Prefault) -> Result<(), u64> {
    if dst.is_empty() {
        return Ok(());
    }
    if !validate_user_range(src_user, dst.len(), prefault) {
        return Err(EFAULT);
    }
    // SAFETY: as `copy_to_user_with`, with the roles reversed.
    unsafe { copy_from_user_safe(dst.as_mut_ptr(), src_user as *const u8, dst.len()) }
}

/// Write one ABI value out to user memory. The length is `size_of::<T>()`, which
/// kills the `(&raw const v).cast::<u8>()` + separately-written-size pairing.
///
/// `T` must be a plain ABI type — `#[repr(C)]` integers and arrays thereof, which
/// is what every call site passes.
pub fn write_user_val<T: Copy>(dst_user: u64, val: &T) -> Result<(), u64> {
    write_user_val_with(dst_user, val, Prefault::Yes)
}

/// [`write_user_val`] with an explicit prefault choice. See [`Prefault`] — the
/// callers that need [`Prefault::No`] are holding a spinlock with IRQs masked.
pub fn write_user_val_with<T: Copy>(
    dst_user: u64,
    val: &T,
    prefault: Prefault,
) -> Result<(), u64> {
    // SAFETY: reading `size_of::<T>()` bytes out of a live `&T` is in bounds.
    let bytes = unsafe {
        core::slice::from_raw_parts((val as *const T).cast::<u8>(), core::mem::size_of::<T>())
    };
    copy_to_user_with(dst_user, bytes, prefault)
}

/// Byte view of a slice of plain ABI values, for copying an *array* out to user
/// memory (`epoll_event[]`, `pollfd[]`, an `fd_set`'s `u64` words).
///
/// Exists so the call site can keep its own length arithmetic — `ready_count *
/// EPOLL_EVENT_SIZE`, `fd_set_bytes` — visible and in bytes, while the pointer cast
/// happens once here instead of at every call site. Slice it before passing it on:
/// `copy_to_user(p, &as_user_bytes(&events)[..n * SIZE])`.
#[must_use]
pub fn as_user_bytes<T: Copy>(vals: &[T]) -> &[u8] {
    // SAFETY: `size_of::<T>() * len` bytes of a live slice, read-only, and `u8` has
    // no alignment requirement.
    unsafe {
        core::slice::from_raw_parts(
            vals.as_ptr().cast::<u8>(),
            core::mem::size_of_val(vals),
        )
    }
}

/// Mutable byte view of a slice of plain ABI values, for copying an array in.
///
/// Same requirement as [`read_user_into`]: `T` must be plain ABI data, since user
/// bytes land on it verbatim.
#[must_use]
pub fn as_user_bytes_mut<T: Copy>(vals: &mut [T]) -> &mut [u8] {
    let len = core::mem::size_of_val(vals);
    // SAFETY: `len` bytes of a live mutable slice; `u8` has no alignment requirement
    // and the borrow is exclusive for the lifetime of the returned slice.
    unsafe { core::slice::from_raw_parts_mut(vals.as_mut_ptr().cast::<u8>(), len) }
}

/// Read one ABI value in from user memory, over an existing `T`.
///
/// Takes `&mut T` rather than returning `T` so no value has to be fabricated from
/// user bytes: the caller's `T` is already valid, and on `Err` it is unmodified
/// only up to the byte the fault hit — check the result before trusting it. Same
/// plain-ABI-type requirement as [`write_user_val`].
pub fn read_user_into<T: Copy>(dst: &mut T, src_user: u64) -> Result<(), u64> {
    read_user_into_with(dst, src_user, Prefault::Yes)
}

/// [`read_user_into`] with an explicit prefault choice.
///
/// [`Prefault::No`] is what a caller inside a spinlock-with-IRQs-masked critical
/// section must pass — `src/syscall/sync.rs`'s futex-word reads and
/// `src/syscall/msgqueue.rs`'s in-hold message copies. Both rely on the range
/// having been prefaulted *before* the lock was taken; the range check itself is a
/// read-only page-table walk and is safe to run under the hold.
pub fn read_user_into_with<T: Copy>(
    dst: &mut T,
    src_user: u64,
    prefault: Prefault,
) -> Result<(), u64> {
    // SAFETY: writing `size_of::<T>()` bytes into a live `&mut T` is in bounds.
    let bytes = unsafe {
        core::slice::from_raw_parts_mut((dst as *mut T).cast::<u8>(), core::mem::size_of::<T>())
    };
    copy_from_user_with(bytes, src_user, prefault)
}

/// Copy from user memory to kernel memory safely.
/// Returns Ok(()) on success, Err(EFAULT) on failure.
///
/// **Raw primitive — prefer [`copy_from_user`].** This checks nothing: it is the
/// copy loop plus the fault trampoline. Every remaining caller is either the safe
/// wrapper above or a path that documents why it validates differently.
pub unsafe fn copy_from_user_safe(dst: *mut u8, src: *const u8, len: usize) -> Result<(), u64> {
    set_user_copy_fault_handler(__arch_copy_user_fault as *const () as usize as u64);

    // Ensure compiler doesn't reorder these calls
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);

    // SAFETY: the fault trampoline registered above turns an unmapped user
    // address into EFAULT instead of an EL1 panic; the caller owns the kernel-side
    // length invariant (the safe wrappers take slices so it cannot disagree).
    let res = unsafe { __arch_copy_user_memory(dst, src, len) };

    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    set_user_copy_fault_handler(0);

    if res == 0 {
        Ok(())
    } else {
        Err(res)
    }
}

/// Copy to user memory from kernel memory safely.
/// Returns Ok(()) on success, Err(EFAULT) on failure.
///
/// **Raw primitive — prefer [`copy_to_user`].** Note this is literally the same
/// routine as [`copy_from_user_safe`]: the loop is symmetric, so the direction lives
/// only in the name and swapping the arguments compiles.
pub unsafe fn copy_to_user_safe(dst: *mut u8, src: *const u8, len: usize) -> Result<(), u64> {
    // One routine serves both directions: the copy loop is symmetric, so this is
    // an argument swap and not a second implementation. Worth stating because it
    // is also what made widening that loop (64/16/8-byte chunks, 2026-08-27) a
    // single edit that sped up user->kernel and kernel->user alike.
    unsafe { copy_from_user_safe(dst, src, len) }
}

/// Differential sweep: does the widened copy loop agree with the byte loop?
///
/// `docs/archive/BUSYBOX_HASH_MISCOMPUTE.md` A/B'd the 2026-08-27 widening as the
/// cause of a silent read corruption, but eyeballing the loop finds no defect and
/// a length/tier bug would corrupt deterministically rather than ~50 % of the
/// time. This settles the question the only way that scales: run **both**
/// routines over the cross-product of source alignment, destination alignment and
/// length, on KERNEL memory only, and compare every byte.
///
/// Kernel memory on purpose. It takes user pages, page faults, the prefault path
/// and the emulator's user-page handling entirely out of the picture, so:
///
///   * mismatches here  => the asm itself is wrong, and this names the exact
///     (src_align, dst_align, len) that breaks it;
///   * no mismatches    => the asm is correct in isolation and the corruption
///     lives in its interaction with user pages — which is where to look next.
///
/// Returns `(cases_checked, mismatches, first_bad_key)`, where the key packs
/// `src_align << 32 | dst_align << 16 | len` for the first disagreement.
#[cfg(target_os = "none")]
#[must_use]
pub fn copy_loop_differential_sweep() -> (u32, u32, u64) {
    const BUF: usize = 8192;
    // Static, not heap: this runs from a boot test and must not depend on the
    // allocator being healthy.
    static mut SRC: [u8; BUF] = [0; BUF];
    static mut GOT_WIDE: [u8; BUF] = [0; BUF];
    static mut GOT_BYTE: [u8; BUF] = [0; BUF];

    // SAFETY: single-threaded boot-test context; these are private to this fn.
    let (src, wide, byte) = unsafe {
        (
            &mut *core::ptr::addr_of_mut!(SRC),
            &mut *core::ptr::addr_of_mut!(GOT_WIDE),
            &mut *core::ptr::addr_of_mut!(GOT_BYTE),
        )
    };
    for (i, b) in src.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(31).wrapping_add(7);
    }

    let mut checked = 0u32;
    let mut bad = 0u32;
    let mut first_bad = 0u64;

    // Alignments 0..=71 cover every byte offset within a 64-byte group, plus a
    // little past it so a group can start mid-way through the next one.
    //
    // Lengths are chosen so that every tier boundary and every tail remainder is
    // hit exactly. The widened loop is `64-byte groups -> 16-byte pairs -> one
    // 8-byte -> byte tail`, so what matters is which tiers a length activates and
    // what it leaves over for the next one:
    const LENS: [usize; 22] = [
        1,    // byte tail only, minimum non-empty copy
        2,    // byte tail, more than one iteration of it
        7,    // byte tail at its maximum (8 would promote to the 8-byte tier)
        8,    // the 8-byte tier exactly, nothing left over
        9,    // 8-byte tier + 1 byte of tail
        15,   // 8-byte tier + 7 bytes of tail: the largest pre-16 case
        16,   // one 16-byte pair exactly, no tail
        17,   // one 16-byte pair + 1 byte
        23,   // one pair + 7 bytes: pair tier then the full byte tail
        31,   // one pair + 8 + 7: every sub-64 tier active at once
        32,   // two 16-byte pairs exactly
        63,   // the largest length that never enters the 64-byte tier
        64,   // one 64-byte group exactly — the first length that does
        65,   // one group + 1 byte, i.e. group then straight to the byte tail
        79,   // one group + 15: group, then 8-byte tier, then tail
        80,   // one group + one pair exactly
        127,  // one group + 63: group then every lower tier
        128,  // two groups exactly
        129,  // two groups + 1
        1023, // 15 groups + 63: long run, then every tier drains
        1024, // 16 groups exactly, a page-quarter, all tiers idle after
        4097, // one page + 1 byte: the smallest length whose copy must cross a
              // page boundary, which is where a straddling `stp` would land
    ];
    let mut sa = 0usize;
    while sa <= 71 {
        let mut da = 0usize;
        while da <= 71 {
            for &len in &LENS {
                if sa + len >= BUF || da + len >= BUF {
                    continue;
                }
                wide[da..da + len].fill(0xA5);
                byte[da..da + len].fill(0xA5);
                // SAFETY: both ranges are in bounds of their static buffers, and
                // kernel memory never faults, so the trampoline is not needed.
                unsafe {
                    let _ = __arch_copy_user_memory(
                        wide.as_mut_ptr().add(da), src.as_ptr().add(sa), len);
                    let _ = __arch_copy_user_memory_bytes(
                        byte.as_mut_ptr().add(da), src.as_ptr().add(sa), len);
                }
                checked += 1;
                if wide[da..da + len] != byte[da..da + len]
                    || wide[da..da + len] != src[sa..sa + len]
                {
                    if bad == 0 {
                        first_bad = ((sa as u64) << 32) | ((da as u64) << 16) | len as u64;
                    }
                    bad += 1;
                }
            }
            da += 1;
        }
        sa += 1;
    }
    (checked, bad, first_bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Only `user_range_ok` is host-testable: everything else here either reads
    // TTBR0, allocates frames or runs the copy asm. That is also where the bugs of
    // this shape live — an off-by-one on the limit, a missed overflow — so the
    // split is the point, not a compromise. See §6.1 of
    // `docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md`.

    // `BYPASS_VALIDATION` is the exception to the note above: it reads no TTBR0 and
    // touches no frames, and its nesting contract is exactly what went wrong when
    // the AP-bit check (§7 of `USER_COPY_FOLD.md`) made a leaked window observable.
    // `current_tid()` is 0 on the host, so these pin the guard's save/restore rather
    // than the per-thread indexing — which is the half that was hand-written and got
    // it wrong.

    /// One test, not two: `current_tid()` is 0 on the host, so every host test
    /// shares slot 0 and two of these running on cargo's test threads would race
    /// each other on the very state they assert about.
    #[test]
    fn bypass_guard_restores_and_nests() {
        assert!(!BYPASS_VALIDATION.load(Ordering::Acquire), "must start off");

        let outer = BypassValidationGuard::new();
        assert!(BYPASS_VALIDATION.load(Ordering::Acquire));
        {
            // The failure this rules out: an inner window closing the outer one,
            // which is precisely what a global flag did across *threads* in
            // `test_futex_wake_one_of_two`.
            let _inner = BypassValidationGuard::new();
            assert!(BYPASS_VALIDATION.load(Ordering::Acquire));
        }
        assert!(
            BYPASS_VALIDATION.load(Ordering::Acquire),
            "the inner guard's drop must not close the outer window"
        );

        drop(outer);
        assert!(
            !BYPASS_VALIDATION.load(Ordering::Acquire),
            "the outermost guard restores what it found, rather than forcing a value"
        );
    }

    #[test]
    fn rejects_the_null_page() {
        assert!(!user_range_ok(0, 1));
        assert!(!user_range_ok(0xFFF, 1));
        assert!(user_range_ok(0x1000, 1));
    }

    #[test]
    fn rejects_a_wrapping_length() {
        // The check exists because `ptr + len` on the raw form would wrap to a
        // small number and pass a naive `end <= limit` test.
        assert!(!user_range_ok(u64::MAX - 4, 16));
        assert!(!user_range_ok(0x1000, usize::MAX));
    }

    #[test]
    fn the_limit_is_the_48_bit_boundary_inclusive() {
        assert!(user_range_ok(USER_VA_LIMIT - 8, 8));
        assert!(!user_range_ok(USER_VA_LIMIT - 8, 9));
        // A TTBR1 (kernel-half) address is past the limit by construction.
        assert!(!user_range_ok(0xFFFF_0000_0000_0000, 1));
    }

    #[test]
    fn go_high_arena_addresses_are_valid() {
        // 0x203e000000 ~= 130 GB: a real goroutine-stack address. Any smaller cap
        // than the 48-bit limit rejects these, which is why the limit is what it is.
        assert!(user_range_ok(0x0000_0020_3e00_0000, 4096));
    }

    #[test]
    fn a_zero_length_range_is_judged_on_its_pointer_alone() {
        assert!(user_range_ok(0x1000, 0));
        assert!(!user_range_ok(0, 0));
        assert!(user_range_ok(USER_VA_LIMIT, 0));
    }
}

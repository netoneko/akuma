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

//! # Values, not just bytes
//!
//! [`read_val`]/[`write_val`] exist because the syscall ABI is full of small
//! fixed-layout structs — `timespec`, `iovec`, `pollfd` fields — that were each
//! being read as two or three separate raw loads. `T` must be plain ABI data
//! (integers and arrays of them): user bytes land on it verbatim, and this
//! module cannot check that for you.

use alloc::vec::Vec;
use core::mem::MaybeUninit;
use core::sync::atomic::Ordering;

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
    // SAFETY: `dst` is a live kernel slice of the stated length; the source is
    // a range-checked user address, and a fault in the copy is recovered by the
    // page-fault handler into an `Err` rather than a halt.
    unsafe { copy_from_user_safe(dst.as_mut_ptr(), ptr as *const u8, dst.len()) }.is_ok()
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
    // SAFETY: as `read_bytes`, with the roles swapped.
    unsafe { copy_to_user_safe(ptr as *mut u8, src.as_ptr(), src.len()) }.is_ok()
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

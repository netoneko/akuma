#![forbid(unsafe_code)]
#![no_std]
//! User-memory access for a kernel that reaches a task's pages through the physmap.
//!
//! Taoxichuan does not dereference user pointers (no `stac`/fault recovery): every access translates the
//! user page to a frame and touches the frame. What is *policy* in that - which ranges a task may name,
//! how a copy is cut at page boundaries, how a string or a pointer array is read and when it is too long -
//! is here, with host tests. What is *effect* stays in the kernel behind [`Walk`]: translating a page,
//! breaking copy-on-write before a write, touching the frame (the only `unsafe`).
//!
//! `amd64/src/uaccess.rs` is a different mechanism (SMAP-bracketed copies recovered by the fault handler,
//! `akuma-user-access`) and is deliberately not folded into this: the two share a *range rule*, not a copy.

extern crate alloc;

use alloc::vec::Vec;

/// First address the user half cannot reach: the canonical boundary.
pub const USER_END: u64 = 0x0000_8000_0000_0000;
/// One page.
pub const PAGE: u64 = 4096;

/// Is `[va, va + len)` wholly inside one of the task's mapped `regions` (`(start, end)` pairs) and in the
/// user half? A zero-length range is always fine; a wrapping or kernel-half range never is.
#[must_use]
pub fn in_regions(regions: &[(u64, u64)], va: u64, len: u64) -> bool {
    if len == 0 {
        return true;
    }
    let Some(end) = va.checked_add(len) else {
        return false;
    };
    if end > USER_END {
        return false;
    }
    regions.iter().any(|&(s, e)| va >= s && end <= e)
}

/// How a kernel reaches one page of the current task. Each call is within a single page.
pub trait Walk {
    /// Fill `out` from the user bytes at `va`; `false` when the page does not translate.
    fn read_page(&self, va: u64, out: &mut [u8]) -> bool;
    /// Store `src` at user `va` (breaking copy-on-write first); `false` when the page does not translate.
    fn write_page(&self, va: u64, src: &[u8]) -> bool;
}

/// Bytes from `va` to the end of its page.
#[must_use]
pub const fn page_remaining(va: u64) -> usize {
    (PAGE - (va & (PAGE - 1))) as usize
}

/// Copy `dst.len()` bytes in from user `src_va`, one page at a time. `false` if any page does not
/// translate (`dst` may then be partly filled).
pub fn copy_from<W: Walk>(w: &W, dst: &mut [u8], src_va: u64) -> bool {
    let mut done = 0usize;
    while done < dst.len() {
        let va = src_va.wrapping_add(done as u64);
        let n = page_remaining(va).min(dst.len() - done);
        if !w.read_page(va, &mut dst[done..done + n]) {
            return false;
        }
        done += n;
    }
    true
}

/// Copy `src` out to user `dst_va`, one page at a time. `false` if any page does not translate.
pub fn copy_to<W: Walk>(w: &W, dst_va: u64, src: &[u8]) -> bool {
    let mut done = 0usize;
    while done < src.len() {
        let va = dst_va.wrapping_add(done as u64);
        let n = page_remaining(va).min(src.len() - done);
        if !w.write_page(va, &src[done..done + n]) {
            return false;
        }
        done += n;
    }
    true
}

/// Why a string or an array read stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// A byte was unreadable (or a pointer was not user memory).
    Bad,
    /// No terminator within the cap: Linux answers `E2BIG`, never `EFAULT`.
    TooLong,
}

/// Read a NUL-terminated string of at most `max` bytes (excluding the NUL), a page at a time.
///
/// Stops reading at the end of each page, so a string that ends just before an unmapped page succeeds
/// instead of failing on a speculative over-read.
pub fn read_cstr<W: Walk>(w: &W, va: u64, max: usize) -> Result<Vec<u8>, Fault> {
    let mut out = Vec::new();
    let mut at = va;
    let mut chunk = [0u8; 256];
    while out.len() < max {
        let want = page_remaining(at).min(chunk.len()).min(max - out.len());
        if !w.read_page(at, &mut chunk[..want]) {
            return Err(Fault::Bad);
        }
        if let Some(nul) = chunk[..want].iter().position(|&b| b == 0) {
            out.extend_from_slice(&chunk[..nul]);
            return Ok(out);
        }
        out.extend_from_slice(&chunk[..want]);
        at = at.wrapping_add(want as u64);
    }
    Err(Fault::TooLong)
}

/// Read a NULL-terminated array of user pointers (`argv`/`envp` shape): at most `max` entries before the
/// terminator, each entry checked with `is_user(ptr, 1)`.
pub fn read_ptr_array<W: Walk>(
    w: &W,
    arr_va: u64,
    max: usize,
    is_user: impl Fn(u64, u64) -> bool,
) -> Result<Vec<u64>, Fault> {
    let mut out = Vec::new();
    for i in 0..max {
        let mut slot = [0u8; 8];
        if !copy_from(w, &mut slot, arr_va.wrapping_add((i as u64) * 8)) {
            return Err(Fault::Bad);
        }
        let p = u64::from_le_bytes(slot);
        if p == 0 {
            return Ok(out);
        }
        out.push(p);
        if !is_user(p, 1) {
            return Err(Fault::Bad);
        }
    }
    Err(Fault::TooLong)
}

#[cfg(test)]
mod tests;

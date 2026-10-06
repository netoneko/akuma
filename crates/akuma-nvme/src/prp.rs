//! Physical Region Page entries for one physically contiguous buffer.
//!
//! PRP1 may start anywhere dword-aligned; every later entry is a whole 4 KiB
//! page. Up to two pages fit in PRP1/PRP2; beyond that PRP2 points at a list.
//! This driver never chains lists, so one list page bounds a transfer at
//! `512 * 4 KiB` plus the first page — far past any `MDTS` met so far.

/// The memory page size this driver programs (`CC.MPS = 0`).
pub const PAGE: u64 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrpError {
    Empty,
    /// PRP1 must be dword aligned.
    Misaligned,
    /// The list page must itself be page aligned.
    ListMisaligned,
    /// More pages than the list holds.
    TooLong,
}

/// `(prp1, prp2)` for `len` bytes at `phys`, filling `list` (which lives at
/// physical `list_phys`) when a list is needed. Returns how many list entries
/// were written as the third value.
pub fn plan(phys: u64, len: usize, list_phys: u64, list: &mut [u64]) -> Result<(u64, u64, usize), PrpError> {
    if len == 0 {
        return Err(PrpError::Empty);
    }
    if phys & 3 != 0 {
        return Err(PrpError::Misaligned);
    }
    let first = (PAGE - phys % PAGE) as usize;
    if len <= first {
        return Ok((phys, 0, 0));
    }
    let next = (phys & !(PAGE - 1)) + PAGE;
    let rest_pages = (len - first).div_ceil(PAGE as usize);
    if rest_pages == 1 {
        return Ok((phys, next, 0));
    }
    if !list_phys.is_multiple_of(PAGE) {
        return Err(PrpError::ListMisaligned);
    }
    if rest_pages > list.len() || rest_pages > (PAGE / 8) as usize {
        return Err(PrpError::TooLong);
    }
    for (i, slot) in list.iter_mut().take(rest_pages).enumerate() {
        *slot = next + i as u64 * PAGE;
    }
    Ok((phys, list_phys, rest_pages))
}

//! A core list (`kvmcores=4-7`, `0,2,4-5`) as a bit mask.
//!
//! The kernel parameter that says which host cores the hypervisor may run on. A mask rather
//! than a list so placement is a bit test; 64 cores is far past anything this kernel boots.

/// Highest core number a mask can hold.
pub const MAX_CORE: u32 = 63;

/// Parse `a`, `a-b`, or a comma-separated mix, into a mask. `None` for an empty list, a
/// reversed range, a core past [`MAX_CORE`], or anything that is not a number.
#[must_use]
pub fn parse(list: &str) -> Option<u64> {
    let mut mask = 0u64;
    for part in list.split(',') {
        let (lo, hi) = if let Some((a, b)) = part.split_once('-') {
            (a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)
        } else {
            let n = part.parse::<u32>().ok()?;
            (n, n)
        };
        if lo > hi || hi > MAX_CORE {
            return None;
        }
        for c in lo..=hi {
            mask |= 1 << c;
        }
    }
    (mask != 0).then_some(mask)
}

/// The lowest core in `mask` that `usable` accepts (the VM task goes there).
pub fn first(mask: u64, mut usable: impl FnMut(u32) -> bool) -> Option<u32> {
    (0..=MAX_CORE).find(|&c| mask & (1 << c) != 0 && usable(c))
}

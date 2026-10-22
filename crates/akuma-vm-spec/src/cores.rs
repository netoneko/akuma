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

/// Highest number of virtual processors a placement is computed for (`spec::MAX_VCPUS`).
pub const MAX_PLACED: usize = 4;

/// Where each virtual processor runs.
///
/// `wanted` is the VM's own core mask (`vm_cores`; 0 = no preference), `allowed` the machine's
/// (`kvmcores=`); only cores in both, and that `usable` accepts (SVM enabled), are candidates.
/// With no preference every allowed core is. Virtual processor `i` goes to the `i`-th
/// candidate, wrapping when there are fewer cores than processors (those share a core and take
/// turns, as every processor did before placement existed). A mask with no usable allowed core is
/// no preference (the machine's allowed cores), and `None` if none of those is usable either.
#[must_use]
pub fn place(wanted: u64, allowed: u64, mut usable: impl FnMut(u32) -> bool, vcpus: usize) -> Option<[u8; MAX_PLACED]> {
    let mut cand = [0u8; MAX_PLACED];
    let mut n = 0;
    // The VM's own mask first; a mask that names no usable allowed core is no preference.
    for pool in [if wanted == 0 { allowed } else { wanted & allowed }, allowed] {
        for c in 0..=MAX_CORE {
            if pool & (1 << c) != 0 && usable(c) && n < MAX_PLACED {
                cand[n] = c as u8;
                n += 1;
            }
        }
        if n > 0 {
            break;
        }
    }
    if n == 0 {
        return None;
    }
    let mut out = [0u8; MAX_PLACED];
    for (i, o) in out.iter_mut().enumerate().take(vcpus.min(MAX_PLACED)) {
        *o = cand[i % n];
    }
    Some(out)
}

/// `mask` as the list [`parse`] reads back (`4-7`, `0,2,4-5`), into `out`; the length written,
/// or `None` if it does not fit or the mask is empty.
#[must_use]
pub fn format(mask: u64, out: &mut [u8]) -> Option<usize> {
    let mut n = 0;
    let mut push = |b: &[u8], n: &mut usize| -> Option<()> {
        out.get_mut(*n..*n + b.len())?.copy_from_slice(b);
        *n += b.len();
        Some(())
    };
    let mut c = 0u32;
    let mut first_run = true;
    while c <= MAX_CORE {
        if mask & (1 << c) == 0 {
            c += 1;
            continue;
        }
        let lo = c;
        while c < MAX_CORE && mask & (1 << (c + 1)) != 0 {
            c += 1;
        }
        if !first_run {
            push(b",", &mut n)?;
        }
        first_run = false;
        let mut digits = [0u8; 4];
        push(num(lo, &mut digits), &mut n)?;
        if c > lo {
            push(b"-", &mut n)?;
            push(num(c, &mut digits), &mut n)?;
        }
        c += 1;
    }
    (n > 0).then_some(n)
}

fn num(mut v: u32, d: &mut [u8; 4]) -> &[u8] {
    let mut i = d.len();
    loop {
        i -= 1;
        d[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    &d[i..]
}

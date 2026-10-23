//! Per-thread CPU affinity: the pure half of `sched_setaffinity` / `sched_getaffinity`.
//!
//! A mask is a `u64` (bit `c` = core `c`; [`crate::cores::MAX_CORE`] is 63), `0` meaning "no
//! preference". The scheduler stores one per thread; this module decides what a request means
//! and whether a thread may run on a core. The effect (storing, migrating) stays in `sched.rs`.

/// Why a `sched_setaffinity` request is refused (`EINVAL` both).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The user's mask buffer is shorter than one `u64` (Linux: smaller than the kernel's mask).
    TooShort,
    /// A bit past core 63 is set, or no requested core is online.
    NoCore,
}

/// Mask of the first `online` cores (all 64 when `online >= 64`).
#[must_use]
pub fn online_mask(online: usize) -> u64 {
    if online >= 64 { u64::MAX } else { (1u64 << online) - 1 }
}

/// Read a user-supplied cpu_set (little-endian bytes) as a mask.
///
/// Bytes past the eighth must be zero: this kernel has no core that could use them, and silently dropping a named core would
/// make `taskset` report success for a placement that never happens.
pub fn decode(bytes: &[u8]) -> Result<u64, Refused> {
    let Some((head, tail)) = bytes.split_first_chunk::<8>() else { return Err(Refused::TooShort) };
    if tail.iter().any(|&b| b != 0) {
        return Err(Refused::NoCore);
    }
    Ok(u64::from_le_bytes(*head))
}

/// The affinity to store for a request: the mask intersected with the online cores.
///
/// Empty is refused; a mask covering every online core is stored as `0` ("any"), so a thread that was
/// told "everywhere" keeps following cores that come up later.
pub fn effective(requested: u64, online: usize) -> Result<u64, Refused> {
    let all = online_mask(online);
    let m = requested & all;
    if m == 0 {
        Err(Refused::NoCore)
    } else if m == all {
        Ok(0)
    } else {
        Ok(m)
    }
}

/// The mask `sched_getaffinity` reports: the stored one, or every online core.
#[must_use]
pub fn report(stored: u64, online: usize) -> u64 {
    if stored == 0 { online_mask(online) } else { stored & online_mask(online) }
}

/// May a thread run on core `cpu`?
///
/// `pinned` is the kernel's hard single-core pin (`None` = unpinned), `affinity` the thread's
/// mask (`0` = any), `reserved` the cores kept for tasks that ask for them. A reserved core runs
/// a thread only if it is pinned there or its affinity names that core; an unmasked, unpinned
/// thread never lands on one.
#[must_use]
pub fn can_run(pinned: Option<u32>, affinity: u64, reserved: u64, cpu: u32) -> bool {
    let bit = 1u64 << (cpu & 63);
    if pinned.is_some_and(|p| p != cpu) {
        return false;
    }
    if affinity != 0 && affinity & bit == 0 {
        return false;
    }
    if reserved & bit != 0 {
        return pinned == Some(cpu) || affinity & bit != 0;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_reads_little_endian_and_refuses_unusable_bits() {
        assert_eq!(decode(&[0xf0, 0, 0, 0, 0, 0, 0, 0]), Ok(0xf0));
        assert_eq!(decode(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]), Ok(1));
        assert_eq!(decode(&[1, 0, 0, 0]), Err(Refused::TooShort));
        assert_eq!(decode(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 1]), Err(Refused::NoCore));
    }

    #[test]
    fn effective_clips_to_online_and_normalises_everywhere() {
        assert_eq!(effective(0b1100, 4), Ok(0b1100));
        assert_eq!(effective(0b1111, 4), Ok(0));
        assert_eq!(effective(u64::MAX, 4), Ok(0));
        assert_eq!(effective(0b1_0000, 4), Err(Refused::NoCore));
        assert_eq!(effective(0, 4), Err(Refused::NoCore));
        assert_eq!(effective(0b1_0100, 4), Ok(0b0100));
    }

    #[test]
    fn report_is_the_stored_mask_or_every_online_core() {
        assert_eq!(report(0, 4), 0b1111);
        assert_eq!(report(0b0110, 4), 0b0110);
        assert_eq!(report(0, 64), u64::MAX);
    }

    #[test]
    fn an_unconstrained_thread_runs_anywhere_but_a_reserved_core() {
        assert!(can_run(None, 0, 0, 3));
        assert!(!can_run(None, 0, 1 << 3, 3));
        assert!(can_run(None, 0, 1 << 3, 2));
    }

    #[test]
    fn a_mask_names_the_cores_a_thread_may_use_reserved_or_not() {
        assert!(can_run(None, 1 << 3, 1 << 3, 3));
        assert!(!can_run(None, 1 << 3, 1 << 3, 2));
        assert!(!can_run(None, 0b11 << 4, 0, 2));
        assert!(can_run(None, 0b11 << 4, 0b1111 << 4, 5));
    }

    #[test]
    fn a_hard_pin_wins_and_is_intersected_with_the_mask() {
        assert!(can_run(Some(2), 0, 0, 2));
        assert!(!can_run(Some(2), 0, 0, 1));
        assert!(can_run(Some(2), 0, 1 << 2, 2));
        // A pin to a core the mask excludes runs nowhere rather than escaping the mask.
        assert!(!can_run(Some(2), 1 << 3, 0, 2));
    }
}

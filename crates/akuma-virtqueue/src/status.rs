//! The device-status register and the order its bits are set in.
//!
//! virtio 1.x §3.1.1 lists initialisation as ordered steps, each marked by
//! setting one bit. QEMU ORs whatever is written; **Firecracker accepts only the
//! exact next value** of its transition table and silently discards any other
//! write, leaving the device uninitialised while config reads still look healthy
//! (the first I/O then hangs; `docs/archive/AKUMA_FIRECRACKER_KVM.md`). So the
//! driver writes [`STEPS`] cumulatively, one bit per write, and nothing else.

pub const ACKNOWLEDGE: u32 = 1;
pub const DRIVER: u32 = 2;
pub const DRIVER_OK: u32 = 4;
pub const FEATURES_OK: u32 = 8;
pub const DEVICE_NEEDS_RESET: u32 = 64;
pub const FAILED: u32 = 128;

/// The initialisation milestones, in the only order Firecracker accepts.
/// Between [`FEATURES_OK`] and [`DRIVER_OK`] the driver sets up its queues.
pub const STEPS: [u32; 4] = [ACKNOWLEDGE, DRIVER, FEATURES_OK, DRIVER_OK];

/// The register value after `step` has been reached: every milestone up to and
/// including it. Writing these in order is the one-bit-per-write sequence.
#[must_use]
pub const fn through(step: u32) -> u32 {
    let mut v = 0;
    let mut i = 0;
    while i < STEPS.len() {
        v |= STEPS[i];
        if STEPS[i] == step {
            return v;
        }
        i += 1;
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_step_adds_exactly_one_bit_to_the_one_before() {
        // Firecracker's table: 0 -> 1 -> 3 -> 11 -> 15.
        let seq: Vec<u32> = STEPS.iter().map(|&s| through(s)).collect();
        assert_eq!(seq, [1, 3, 11, 15]);
        let mut prev = 0;
        for v in seq {
            assert_eq!((v & !prev).count_ones(), 1);
            assert_eq!(v & prev, prev);
            prev = v;
        }
    }
}

//! Akuma multikernel (one-kernel-per-core) coordination — the pure, host-testable
//! half of the SMP subsystem (docs/MULTIKERNEL.md).
//!
//! It deliberately contains NO kernel/arch dependencies (no asm, PSCI, page tables,
//! console), so it compiles for both the AArch64 kernel and the host, and its
//! algorithms can be unit-tested and *simulated* with no QEMU.
//!
//! - [`Ring`] — the lock-free MPSC inbox (the only cross-core data path).
//! - [`MachineConfig`]/[`CoreConfig`] — the shared descriptor the BSP fills and every
//!   core maps.
//! - [`partition`] — RAM-carving math.
//!
//! The kernel glue (asm trampoline, PSCI `CPU_ON`, per-core page tables, the pump that
//! drives this logic) lives in `src/smp.rs` and depends on this crate.

#![no_std]

#[cfg(test)]
extern crate std;

mod descriptor;
mod ring;

pub use descriptor::{
    CoreConfig, MachineConfig, ENF_FAULTED, ENF_LEAKED, ENF_TESTING, MAGIC, MAX_CORES,
    STATE_BOOTING, STATE_OFFLINE, STATE_ONLINE,
};
pub use ring::{Ring, MSG_MEMORY_OFFER, MSG_PRESSURE_REPORT, RING_CAP};

/// Carve detected RAM into `num_cores` disjoint, 2 MiB-aligned partitions
/// (docs/MULTIKERNEL.md §4.1).
///
/// Core `i` owns `[ram_base + i*slice, …)`; the last core absorbs the remainder.
/// The BSP's partition (core 0) contains the kernel image (loaded at
/// `ram_base + 1 MiB`). Returns `(base, len)` per core.
///
/// Read at RUNTIME (never a compile-time const) so memory renegotiation (§9) stays
/// a protocol addition, not a format change.
#[must_use]
pub fn partition(ram_base: usize, ram_size: usize, num_cores: usize) -> [(u64, u64); MAX_CORES] {
    const ALIGN: usize = 2 * 1024 * 1024;
    let mut parts = [(0u64, 0u64); MAX_CORES];
    if num_cores == 0 {
        return parts;
    }
    let slice = ((ram_size / num_cores) / ALIGN) * ALIGN;
    for (i, p) in parts.iter_mut().enumerate().take(num_cores) {
        let base = ram_base + i * slice;
        let len = if i == num_cores - 1 {
            (ram_base + ram_size) - base // last core absorbs the remainder
        } else {
            slice
        };
        *p = (base as u64, len as u64);
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_equal_split_aligned() {
        // 2 GiB, 4 cores → 512 MiB each, all 2 MiB-aligned.
        let p = partition(0x4000_0000, 2048 * 1024 * 1024, 4);
        assert_eq!(p[0], (0x4000_0000, 512 * 1024 * 1024));
        assert_eq!(p[1], (0x6000_0000, 512 * 1024 * 1024));
        assert_eq!(p[2], (0x8000_0000, 512 * 1024 * 1024));
        assert_eq!(p[3], (0xa000_0000, 512 * 1024 * 1024));
    }

    #[test]
    fn partition_remainder_to_last_core() {
        // 100 MiB, 3 cores: slice = floor(33.3/2)*2 = 32 MiB; last gets the rest.
        let total = 100 * 1024 * 1024;
        let p = partition(0x4000_0000, total, 3);
        let slice = 32 * 1024 * 1024u64;
        assert_eq!(p[0].1, slice);
        assert_eq!(p[1].1, slice);
        // last core covers everything left over
        assert_eq!(p[2].1, total as u64 - 2 * slice);
        // partitions are contiguous and cover exactly [base, base+total)
        assert_eq!(p[0].0, 0x4000_0000);
        assert_eq!(p[1].0, p[0].0 + p[0].1);
        assert_eq!(p[2].0, p[1].0 + p[1].1);
        assert_eq!(p[2].0 + p[2].1, 0x4000_0000 + total as u64);
    }

    #[test]
    fn partition_single_core_gets_all() {
        let total = 256 * 1024 * 1024;
        let p = partition(0x4000_0000, total, 1);
        assert_eq!(p[0], (0x4000_0000, total as u64));
    }
}

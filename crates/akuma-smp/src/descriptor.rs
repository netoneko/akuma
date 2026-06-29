//! The shared machine descriptor.
//!
//! The BSP fills it before waking secondaries, and every core maps the single page
//! it occupies. The ONLY shared mutable memory in the multikernel — per-core private
//! state lives elsewhere, unmapped by peers.

use core::sync::atomic::{AtomicU32, AtomicU64};

use crate::console_ring::ConsoleRing;
use crate::fwd_bounce::FwdBounce;
use crate::ring::Ring;

/// Maximum physical PEs the descriptor describes.
///
/// QEMU `virt` packs CPU affinity as `aff0 = cpu_index` for the first 16 cores
/// (single cluster), so a core's index into [`MachineConfig::cores`] is
/// `MPIDR_EL1 & 0xff`.
pub const MAX_CORES: usize = 8;

/// Sanity magic so a secondary can confirm it read a real descriptor ("AKUMAMK1").
pub const MAGIC: u64 = 0x414b_554d_414d_4b31;

// Core lifecycle states (`CoreConfig::state`). The BSP watches Offline -> Online.
pub const STATE_OFFLINE: u32 = 0;
pub const STATE_BOOTING: u32 = 1;
pub const STATE_ONLINE: u32 = 2;

// Enforcement self-test outcomes (`MachineConfig::enforcement_results`).
pub const ENF_TESTING: u32 = 0;
/// Cross-core access faulted → isolation is hardware-enforced (the good outcome).
pub const ENF_FAULTED: u32 = 1;
/// Cross-core read SUCCEEDED → isolation leaked (the table is too permissive).
pub const ENF_LEAKED: u32 = 2;

/// Per-core slot in the shared descriptor. `#[repr(C)]` + a fixed layout so the asm
/// trampoline and Rust agree byte-for-byte.
#[repr(C)]
pub struct CoreConfig {
    /// MPIDR_EL1 affinity of this PE (PSCI `CPU_ON` target).
    pub mpidr: u64,
    /// This core's PRIVATE physical partition (read at runtime; §9 renegotiation).
    pub ram_base: u64,
    pub ram_len: u64,
    pub kernel_end: u64,
    /// Per-core isolated boot-stack top, in the core's private chunk.
    pub entry_sp: u64,
    /// Root (L0 PA) of this core's RESTRICTED page table (0 ⇒ none built).
    pub ttbr0_phys: u64,
    /// PA of this core's private PerCpu page.
    pub percpu_phys: u64,
    /// Offline -> Booting -> Online. Cross-core via inner-shareable coherency.
    pub state: AtomicU32,
    /// Explicit tail padding to keep the `#[repr(C)]` layout fixed (not `pub` —
    /// nothing reads it; it only pins the struct size for the asm contract).
    _pad: u32,
}

impl CoreConfig {
    const fn new() -> Self {
        Self {
            mpidr: 0,
            ram_base: 0,
            ram_len: 0,
            kernel_end: 0,
            entry_sp: 0,
            ttbr0_phys: 0,
            percpu_phys: 0,
            state: AtomicU32::new(STATE_OFFLINE),
            _pad: 0,
        }
    }
}

/// Read-only-after-init machine descriptor + the one SHARED page every core maps.
///
/// `align(4096)` rounds the type's size up to a whole page, so the static occupies
/// its own page(s) with no other data sharing them — letting a fully-isolated
/// secondary map *exactly* this region as its shared window.
///
/// **Layout contract:** `enforcement_results` MUST stay the first field (byte offset
/// 0). The asm fault handler (`smp_sync_handler` in the kernel) writes
/// `enforcement_results[idx]` via `TPIDR_EL1` (= descriptor base) `+ idx*4`, relying
/// on offset 0. A host test asserts this.
#[repr(C, align(4096))]
pub struct MachineConfig {
    /// MUST be first (offset 0) — see the layout contract above.
    pub enforcement_results: [AtomicU32; MAX_CORES],
    pub magic: u64,
    pub version: u32,
    pub num_cores: u32,
    /// Self physical address (lets a secondary re-find the page; sanity here).
    pub config_phys_addr: u64,
    pub cores: [CoreConfig; MAX_CORES],
    /// Per-core liveness heartbeat: each core monotonically bumps its own slot.
    /// In the SHARED descriptor (not private) precisely so peers may read it
    /// without violating isolation — a stalled counter ⇒ that core is offline.
    pub heartbeat: [AtomicU64; MAX_CORES],
    /// Per-core message inbox (the synchronous control/protocol data path).
    pub inboxes: [Ring; MAX_CORES],
    /// Per-core async console output ring (§8.2): a non-owner core writes its
    /// console bytes here (fire-and-forget, batched) and the console-owner core
    /// (core 0, UART owner) drains them. Separate from `inboxes` so high-volume
    /// console traffic never floods or delays low-rate control messages.
    pub console_rings: [ConsoleRing; MAX_CORES],
    /// Per-core syscall-forwarding bounce region (§8.1): the shared byte buffer where a
    /// forwarding core's inbound/outbound data meets the owner core, so neither ever
    /// dereferences a pointer into the other's partition. Access is serialized by the
    /// request/reply handshake on `inboxes`.
    pub fwd_bounce: [FwdBounce; MAX_CORES],
    /// Set to 1 by the BSP's persistent forward-server thread once it is live and
    /// draining `inboxes[bsp]` (R4b.2). Secondaries poll this before sending a
    /// post-bringup forward request, so the request is provably serviced by the
    /// long-running thread rather than the transient bringup wait loop.
    pub fwd_server_ready: AtomicU32,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl MachineConfig {
    // The descriptor is only ever a `static` (the per-core console rings make it
    // ~tens of KiB); it is never actually placed on a stack, so the large-array lint
    // on the const initializer below is a false positive here.
    #[must_use]
    #[allow(clippy::large_stack_arrays)]
    pub const fn new() -> Self {
        Self {
            enforcement_results: [const { AtomicU32::new(ENF_TESTING) }; MAX_CORES],
            magic: 0,
            version: 0,
            num_cores: 0,
            config_phys_addr: 0,
            cores: [const { CoreConfig::new() }; MAX_CORES],
            heartbeat: [const { AtomicU64::new(0) }; MAX_CORES],
            inboxes: [const { Ring::new() }; MAX_CORES],
            console_rings: [const { ConsoleRing::new() }; MAX_CORES],
            fwd_bounce: [const { FwdBounce::new() }; MAX_CORES],
            fwd_server_ready: AtomicU32::new(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforcement_results_at_offset_zero() {
        // The asm fault handler depends on this. If a field reorder ever moves it,
        // fail HERE (host) instead of silently corrupting memory in the kernel.
        assert_eq!(core::mem::offset_of!(MachineConfig, enforcement_results), 0);
    }

    #[test]
    fn descriptor_is_page_aligned_and_whole_pages() {
        // align(4096) rounds the size up to whole pages; the kernel maps exactly
        // that many pages as the shared window (`map_range_4k(cfg_pa, cfg_len)`), so
        // the size MUST stay a page multiple. With the per-core console rings
        // (CONSOLE_RING_CAP each) it is now several pages, not one.
        let size = core::mem::size_of::<MachineConfig>();
        assert_eq!(core::mem::align_of::<MachineConfig>(), 4096);
        assert_eq!(size % 4096, 0, "descriptor must be a whole number of pages");
        // The console rings dominate the size; sanity-bound it so an accidental
        // blow-up (e.g. a giant CONSOLE_RING_CAP) is caught here, not at boot.
        let upper = (MAX_CORES * (crate::CONSOLE_RING_CAP + crate::FWD_BOUNCE_CAP)) + 8192;
        assert!(size <= upper, "descriptor {size} bytes exceeds {upper}");
    }

    #[test]
    fn fresh_descriptor_state_is_offline() {
        let c = MachineConfig::new();
        assert_eq!(c.magic, 0);
        for core in &c.cores {
            assert_eq!(core.state.load(core::sync::atomic::Ordering::Relaxed), STATE_OFFLINE);
        }
    }
}

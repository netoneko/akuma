//! Locating virtio-mmio devices on the QEMU virt machine.
//!
//! There were four copies of this scan: `src/block.rs`, `src/rng.rs`,
//! `src/audio.rs` and `akuma-net`'s `smoltcp_net.rs`, plus a fifth copy of the
//! address table written inline in `src/main.rs`. CPD caught the three that
//! spelled the table as a `const` and missed the other two — see
//! `docs/archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md` §5.

use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};

/// Number of virtio-mmio slots the QEMU virt machine exposes.
pub const NUM_SLOTS: usize = 8;

/// The QEMU virt machine's eight virtio-mmio slots, 0x200 bytes apart, as seen
/// through the kernel's device mapping (`DEV_VIRTIO_VA`, remapped via L0[1]).
pub const VIRTIO_MMIO_ADDRS: [usize; NUM_SLOTS] = [
    akuma_exec::mmu::DEV_VIRTIO_VA,
    akuma_exec::mmu::DEV_VIRTIO_VA + 0x200,
    akuma_exec::mmu::DEV_VIRTIO_VA + 0x400,
    akuma_exec::mmu::DEV_VIRTIO_VA + 0x600,
    akuma_exec::mmu::DEV_VIRTIO_VA + 0x800,
    akuma_exec::mmu::DEV_VIRTIO_VA + 0xa00,
    akuma_exec::mmu::DEV_VIRTIO_VA + 0xc00,
    akuma_exec::mmu::DEV_VIRTIO_VA + 0xe00,
];

/// Offset of the `DeviceID` register in the virtio-mmio layout.
const VIRTIO_MMIO_DEVICE_ID: usize = 0x008;

/// virtio device IDs (virtio 1.1 §5). Previously spelled three different ways
/// across the copies: a named constant in `block.rs`/`rng.rs`/`audio.rs` and a
/// bare `1` in `smoltcp_net.rs`.
pub mod device_id {
    pub const NET: u32 = 1;
    pub const BLOCK: u32 = 2;
    pub const RNG: u32 = 4;
    pub const SOUND: u32 = 25;
}

/// Read the `DeviceID` of the virtio-mmio slot at `addr`.
fn device_id_at(addr: usize) -> u32 {
    // SAFETY: `addr` is one of `VIRTIO_MMIO_ADDRS`, all of which are inside the
    // device mapping the kernel establishes before any driver init runs. The
    // DeviceID register is read-only and reading it has no side effects.
    unsafe { core::ptr::read_volatile((addr + VIRTIO_MMIO_DEVICE_ID) as *const u32) }
}

/// Every virtio-mmio slot paired with the device id it advertises, in slot order.
///
/// For callers that must reason about the *set* of devices rather than find the
/// first of a kind: `akuma-rump` binds the **second** virtio-net (the first is
/// smoltcp's NIC0), so it cannot use [`find`].
#[must_use]
pub fn scan() -> [(usize, u32); NUM_SLOTS] {
    VIRTIO_MMIO_ADDRS.map(|addr| (addr, device_id_at(addr)))
}

/// Find the first virtio-mmio slot advertising `device_id`, returning
/// `(slot_index, base_address)`.
///
/// For drivers that do **not** use `virtio-drivers`' transport — `rng` hand-rolls
/// its virtqueue against the raw base address — this is the whole probe. Drivers
/// that do want a transport should call [`probe`] instead.
#[must_use]
pub fn find(device_id: u32) -> Option<(usize, usize)> {
    VIRTIO_MMIO_ADDRS
        .iter()
        .enumerate()
        .find(|&(_, &addr)| device_id_at(addr) == device_id)
        .map(|(i, &addr)| (i, addr))
}

/// Find the first virtio-mmio slot advertising `device_id` and build an
/// [`MmioTransport`] over it, returning `(slot_index, transport)`.
///
/// A slot whose header does not yield a working transport is **skipped** and the
/// scan continues, which is what `block.rs` and `audio.rs` did. `smoltcp_net.rs`
/// instead propagated that failure and abandoned the whole scan; converging on
/// skip-and-continue is strictly more forgiving, and the reason the two differed
/// looks like drift rather than intent. The skip is logged so a transport that
/// fails on the slot you cared about is still visible in the boot log — losing
/// that was the one real cost of the reconciliation.
#[must_use]
pub fn probe(device_id: u32) -> Option<(usize, MmioTransport)> {
    probe_with(device_id, |i, transport| Some((i, transport)))
}

/// Like [`probe`], but lets the caller try to build a device from each matching
/// slot and **keep scanning** if that fails.
///
/// `make` returning `None` means "this slot did not work out"; the scan moves on
/// to the next slot advertising `device_id`. This is the behaviour `block.rs` and
/// `audio.rs` had before they moved here — a slot whose `VirtIOBlk::new` /
/// `VirtIOSound::new` failed did not abort the search — and it is preserved
/// deliberately rather than collapsed into [`probe`], which would have quietly
/// turned "try the next virtio-blk" into "give up".
pub fn probe_with<T>(
    device_id: u32,
    mut make: impl FnMut(usize, MmioTransport) -> Option<T>,
) -> Option<T> {
    for (i, &addr) in VIRTIO_MMIO_ADDRS.iter().enumerate() {
        if device_id_at(addr) != device_id {
            continue;
        }

        let Some(header) = core::ptr::NonNull::new(addr as *mut VirtIOHeader) else {
            continue;
        };

        // SAFETY: `header` points at a virtio-mmio header inside the device
        // mapping whose DeviceID we just matched, so the transport is being
        // built over a real device of the expected kind.
        let Ok(transport) = (unsafe { MmioTransport::new(header) }) else {
            crate::safe_print!(64, "[virtio] slot {i}: device {device_id} transport init failed\n");
            continue;
        };

        if let Some(made) = make(i, transport) {
            return Some(made);
        }
    }
    None
}

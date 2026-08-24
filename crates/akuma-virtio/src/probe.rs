//! Locating virtio-mmio devices on the QEMU virt machine.
//!
//! There were four copies of this scan: `src/block.rs`, `src/rng.rs`,
//! `src/audio.rs` and `akuma-net`'s `smoltcp_net.rs`, plus a fifth copy of the
//! address table written inline in `src/main.rs`. CPD caught the three that
//! spelled the table as a `const` and missed the other two — see
//! `docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §5.

use akuma_primitives::mmio::MmioReg;
use crate::transport::SteppedMmioTransport;
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};

/// Upper bound on virtio-mmio slots. The *actual* count is
/// [`akuma_primitives::addr::virtio_slots`], which the machine sets during early
/// boot; this only sizes the fixed-capacity array [`scan`] returns.
pub const MAX_SLOTS: usize = 8;

/// The machine's virtio-mmio slots, as seen through the kernel's device mapping
/// (`DEV_VIRTIO_VA`, remapped via L0[1]).
///
/// This used to be a `const` array with a hardcoded 0x200 stride, which is QEMU
/// virt's packing — eight slots inside one 4 KiB page. Firecracker uses a
/// `MMIO_LEN` (0x1000) stride and one page per device, so both the stride and the
/// count are runtime values now. The probe *logic* below never cared: it walks
/// whatever addresses it is handed.
#[must_use]
pub fn slot_addrs() -> [usize; MAX_SLOTS] {
    let mut out = [0usize; MAX_SLOTS];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = akuma_primitives::addr::virtio_slot_va(i);
    }
    out
}

/// Kernel VA of virtio-mmio slot `i`.
#[inline]
#[must_use]
pub fn slot_addr(i: usize) -> usize {
    akuma_primitives::addr::virtio_slot_va(i)
}

/// How many slots to actually walk.
#[inline]
#[must_use]
pub fn num_slots() -> usize {
    akuma_primitives::addr::virtio_slots().min(MAX_SLOTS)
}

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
    // SAFETY: `addr` is one of the machine's virtio slots (see `slot_addr`), all
    // device mapping the kernel establishes before any driver init runs. The
    // DeviceID register is read-only and reading it has no side effects.
    let device_id: MmioReg<u32> = unsafe { MmioReg::new(addr + VIRTIO_MMIO_DEVICE_ID) };
    device_id.read()
}

/// Every virtio-mmio slot paired with the device id it advertises, in slot order.
///
/// For callers that must reason about the *set* of devices rather than find the
/// first of a kind: `akuma-rump` binds the **second** virtio-net (the first is
/// smoltcp's NIC0), so it cannot use [`find`].
#[must_use]
pub fn scan() -> [(usize, u32); MAX_SLOTS] {
    let mut out = [(0usize, 0u32); MAX_SLOTS];
    let n = num_slots();
    for (i, entry) in out.iter_mut().enumerate() {
        if i < n {
            let addr = slot_addr(i);
            *entry = (addr, device_id_at(addr));
        }
    }
    out
}

/// Find the first virtio-mmio slot advertising `device_id`, returning
/// `(slot_index, base_address)`.
///
/// For drivers that do **not** use `virtio-drivers`' transport — `rng` hand-rolls
/// its virtqueue against the raw base address — this is the whole probe. Drivers
/// that do want a transport should call [`probe`] instead.
#[must_use]
pub fn find(device_id: u32) -> Option<(usize, usize)> {
    (0..num_slots())
        .map(|i| (i, slot_addr(i)))
        .find(|&(_, addr)| device_id_at(addr) == device_id)
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
pub fn probe(device_id: u32) -> Option<(usize, SteppedMmioTransport<'static>)> {
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
    mut make: impl FnMut(usize, SteppedMmioTransport<'static>) -> Option<T>,
) -> Option<T> {
    for (i, addr) in (0..num_slots()).map(|i| (i, slot_addr(i))) {
        if device_id_at(addr) != device_id {
            continue;
        }

        let Some(header) = core::ptr::NonNull::new(addr as *mut VirtIOHeader) else {
            continue;
        };

        // SAFETY: `header` points at a virtio-mmio header inside the device
        // mapping whose DeviceID we just matched, so the transport is being
        // built over a real device of the expected kind.
        // virtio-drivers 0.13 needs the MMIO region size so it can bound the
        // config space. That is exactly the machine's slot stride: 0x200 on QEMU
        // virt (eight slots packed in one page), 0x1000 under Firecracker.
        let mmio_size = akuma_primitives::addr::virtio_stride();
        let Ok(transport) = (unsafe { MmioTransport::new(header, mmio_size) }) else {
            crate::safe_print!(64, "[virtio] slot {i}: device {device_id} transport init failed\n");
            continue;
        };

        if let Some(made) = make(i, SteppedMmioTransport::new(transport)) {
            return Some(made);
        }
    }
    None
}

/// Visit **every** virtio-mmio slot advertising `device_id`, not just the first.
///
/// `f` receives `(slot_index, transport)` for each match and returns whether to
/// keep scanning (`true`) or stop early (`false`). Same per-slot skip-and-log
/// behaviour as [`probe_with`]: a slot whose transport fails to init is logged
/// and the sweep moves on.
///
/// This is the multi-device discovery primitive — `block::init` uses it to
/// register every virtio-blk on the machine (vda, vdb, …) rather than stopping
/// at the first.
pub fn probe_each(
    device_id: u32,
    mut f: impl FnMut(usize, SteppedMmioTransport<'static>) -> bool,
) {
    for (i, addr) in (0..num_slots()).map(|i| (i, slot_addr(i))) {
        if device_id_at(addr) != device_id {
            continue;
        }

        let Some(header) = core::ptr::NonNull::new(addr as *mut VirtIOHeader) else {
            continue;
        };

        // SAFETY: same as `probe_with` — header points at a virtio-mmio header
        // whose DeviceID we just matched.
        let mmio_size = akuma_primitives::addr::virtio_stride();
        let Ok(transport) = (unsafe { MmioTransport::new(header, mmio_size) }) else {
            crate::safe_print!(64, "[virtio] slot {i}: device {device_id} transport init failed\n");
            continue;
        };

        if !f(i, SteppedMmioTransport::new(transport)) {
            return;
        }
    }
}

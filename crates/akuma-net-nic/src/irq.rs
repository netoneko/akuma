//! The NIC's virtio-mmio interrupt and transmit-queue doorbell.
//!
//! Device-level MMIO, deliberately separate from the smoltcp `Device` impl:
//! `nic_irq_ack` runs from IRQ context, which must never take `NETWORK`.

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

// NIC interrupt
// ============================================================================

/// virtio-mmio `InterruptStatus` — which events the device is signalling.
const VIRTIO_MMIO_INTERRUPT_STATUS: usize = 0x060;
/// virtio-mmio `InterruptACK` — write back the bits read from `InterruptStatus`
/// to de-assert the line.
const VIRTIO_MMIO_INTERRUPT_ACK: usize = 0x064;

/// MMIO base of NIC0, captured during [`init`]. 0 until then.
///
/// Held separately from the `NetRaw` inside `NETWORK` because the IRQ
/// handler must reach it **without taking a lock**: the core it interrupted may
/// be the one holding `NETWORK`, and a handler that blocked on it would wedge
/// the machine. A raw MMIO base in an atomic is the same discipline the timer
/// IRQ uses for the GIC (`docs/reference/subsystems/locking.md`, `no-bkl-irq`).
pub(crate) static NIC_MMIO_BASE: AtomicUsize = AtomicUsize::new(0);

/// Record which virtio-mmio slot the stack bound, and its MMIO base.
///
/// Called once from the stack's `init` **before** the device is moved into the
/// `NETWORK` lock: afterwards the device is only reachable under that lock, and
/// IRQ context must never take it. Both stores are `Release` so a concurrent
/// `nic_irq_ack` either sees a zero base and returns, or sees a fully published
/// one.
pub fn bind(slot: u32, mmio_base: usize) {
    NIC_MMIO_BASE.store(mmio_base, Ordering::Release);
    NIC_SLOT.store(slot, Ordering::Release);
}

/// virtio-mmio slot index of NIC0, or [`NIC_SLOT_NONE`] before [`init`].
pub(crate) static NIC_SLOT: AtomicU32 = AtomicU32::new(NIC_SLOT_NONE);
/// Sentinel for "no NIC found / not initialised yet".
pub const NIC_SLOT_NONE: u32 = u32::MAX;

/// The virtio-mmio slot NIC0 was probed at, for the kernel to derive its GIC
/// INTID from. `None` when there is no NIC or the stack has not initialised.
///
/// The kernel owns the slot-to-INTID mapping (it is a property of the machine,
/// not of this crate) — see `src/main.rs`.
#[must_use]
pub fn nic_slot() -> Option<u32> {
    match NIC_SLOT.load(Ordering::Acquire) {
        NIC_SLOT_NONE => None,
        slot => Some(slot),
    }
}

/// Acknowledge NIC0's pending interrupt. **Safe to call from IRQ context.**
///
/// Reads `InterruptStatus` and writes it straight back to `InterruptACK`, which
/// is all the virtio-mmio spec requires to de-assert a level-triggered line.
/// Deliberately does nothing else: the *value* of the NIC interrupt is that it
/// makes a `WFI` return, so the netpoll loop runs immediately instead of waiting
/// for the next scheduler tick. Draining the queue here would need `NETWORK`,
/// which this context must never take.
///
/// A no-op before [`init`] has recorded a base, so an early spurious interrupt
/// cannot fault on a null pointer.
pub fn nic_irq_ack() {
    let base = NIC_MMIO_BASE.load(Ordering::Acquire);
    if base == 0 {
        return;
    }
    NIC_IRQS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: `base` was recorded from `akuma_virtio::probe`, which only yields
    // addresses inside the kernel's device mapping, and is only ever stored
    // once. Both registers are 32-bit at fixed offsets in the virtio-mmio
    // layout.
    unsafe {
        let status: akuma_primitives::mmio::MmioReg<u32> =
            akuma_primitives::mmio::MmioReg::new(base + VIRTIO_MMIO_INTERRUPT_STATUS);
        let ack: akuma_primitives::mmio::MmioReg<u32> =
            akuma_primitives::mmio::MmioReg::new(base + VIRTIO_MMIO_INTERRUPT_ACK);
        let pending = status.read();
        if pending != 0 {
            ack.write(pending);
        }
    }
}


/// Count of NIC interrupts taken. The first thing to check when a latency fix
/// that depends on the interrupt does not move: if this is 0, the SPI never
/// reached the CPU and the stack is still tick-driven.
static NIC_IRQS: AtomicU64 = AtomicU64::new(0);

/// How many NIC interrupts have been taken since boot.
#[must_use]
pub fn nic_irq_count() -> u64 {
    NIC_IRQS.load(Ordering::Relaxed)
}

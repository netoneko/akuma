//! The NIC's virtio-mmio interrupt and transmit-queue doorbell.
//!
//! Device-level MMIO, deliberately separate from the smoltcp `Device` impl:
//! `nic_irq_ack` runs from IRQ context, which must never take `NETWORK`.

use super::*;

// NIC interrupt
// ============================================================================

/// virtio-mmio `InterruptStatus` — which events the device is signalling.
const VIRTIO_MMIO_INTERRUPT_STATUS: usize = 0x060;
/// virtio-mmio `InterruptACK` — write back the bits read from `InterruptStatus`
/// to de-assert the line.
const VIRTIO_MMIO_INTERRUPT_ACK: usize = 0x064;

/// virtio-mmio `QueueNotify`. Writing a queue index tells the device that queue
/// has new available buffers. Only the async transmit path kicks it by hand.
#[cfg(feature = "net-noalloc")]
const VIRTIO_MMIO_QUEUE_NOTIFY: usize = 0x050;

/// virtio-net's transmit virtqueue index (receive is 0).
#[cfg(feature = "net-noalloc")]
const VIRTIO_NET_QUEUE_TRANSMIT: u32 = 1;

/// MMIO base of NIC0, captured during [`init`]. 0 until then.
///
/// Held separately from the `VirtIONetRaw` inside `NETWORK` because the IRQ
/// handler must reach it **without taking a lock**: the core it interrupted may
/// be the one holding `NETWORK`, and a handler that blocked on it would wedge
/// the machine. A raw MMIO base in an atomic is the same discipline the timer
/// IRQ uses for the GIC (see the `no-bkl-irq` feature notes).
pub(crate) static NIC_MMIO_BASE: AtomicUsize = AtomicUsize::new(0);

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

/// Kick the transmit queue unconditionally.
///
/// `transmit_begin` notifies only when `VirtQueue::should_notify()` allows, and
/// QEMU negotiates `VIRTIO_F_EVENT_IDX`, so that can be false. The blocking
/// `VirtIONetRaw::send` this replaced checks the same flag — but then *spins
/// until the used ring advances*, which waits the suppression out and forces
/// the host to pick the frame up. Async submit has no such backstop, so a
/// suppressed notify leaves the frame sitting in the avail ring.
///
/// Measured cost of not doing this (`[NICSTAT] tx_flight`, 2026-08-19):
/// **90.9 us average submit → completion, 6,486 us worst case**, against a
/// 9.1 us submit — and an HTTP p99 of 6,747 us that tracks the worst case
/// almost exactly.
///
/// A spurious notify is harmless by spec (it is a hint), so this is
/// unconditional rather than trying to second-guess `should_notify`.
#[cfg(feature = "net-noalloc")]
pub(crate) fn nic_kick_tx() {
    let base = NIC_MMIO_BASE.load(Ordering::Acquire);
    if base == 0 {
        return;
    }
    // SAFETY: same base and the same discipline as `nic_irq_ack` — recorded
    // once from `akuma_virtio::probe`, inside the kernel's device mapping.
    // `QueueNotify` is a 32-bit write-only register at a fixed offset.
    unsafe {
        let notify: akuma_primitives::mmio::MmioReg<u32> =
            akuma_primitives::mmio::MmioReg::new(base + VIRTIO_MMIO_QUEUE_NOTIFY);
        notify.write(VIRTIO_NET_QUEUE_TRANSMIT);
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

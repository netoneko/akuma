//! A virtio-mmio transport that steps the status register one bit-set at a time.
//!
//! # Why this exists
//!
//! virtio 1.0 §3.1.1 lists device initialisation as an ordered sequence of steps,
//! each marked by setting a bit in the device status register. It says the driver
//! "MUST update device status, setting bits to indicate the completed steps" —
//! which leaves open whether two steps may be marked in a single write.
//!
//! Hypervisors differ on that, and the difference is fatal rather than cosmetic:
//!
//! - **QEMU** ORs the written bits into its status and never checks the order.
//! - **Firecracker** validates each write against an exact-match transition table
//!   (`src/vmm/src/devices/virtio/transport/mmio.rs`):
//!   ```text
//!   INIT                          -> ACKNOWLEDGE
//!   ACKNOWLEDGE                   -> ACKNOWLEDGE|DRIVER
//!   ACKNOWLEDGE|DRIVER            -> ACKNOWLEDGE|DRIVER|FEATURES_OK
//!   ACKNOWLEDGE|DRIVER|FEATURES_OK-> ACKNOWLEDGE|DRIVER|FEATURES_OK|DRIVER_OK
//!   ```
//!   A write that is not exactly one of those pairs is **discarded with a warning**
//!   and the status is left unchanged.
//!
//! `virtio-drivers` 0.7.5 (`src/transport/mod.rs:74-75`) does:
//!
//! ```text
//! set_status(empty())                  // 0x0
//! set_status(ACKNOWLEDGE | DRIVER)     // 0x3   <-- skips 0x0 -> 0x1
//! ```
//!
//! Under Firecracker the second write is rejected, so status stays at `INIT`
//! forever. Every later queue configuration is then refused for being in
//! "invalid state 0x0", and `activate()` — which Firecracker only runs on the
//! exact transition to `DRIVER_OK` — never happens. The device is never turned on.
//!
//! The failure is deceptive: config-space reads need no handshake, so the device
//! ID and capacity come back correct and the boot log shows a healthy-looking
//! `[Block] Capacity: 2048 MB`. The first real I/O then waits forever, which
//! presents as a hang in ext2 mount rather than a device error.
//! See `docs/archive/AKUMA_FIRECRACKER_KVM.md`.
//!
//! # Why a wrapper rather than a patched dependency
//!
//! Forking `virtio-drivers` for a one-line change would mean carrying a patched
//! crate. Instead this newtype delegates the whole [`Transport`] trait to the
//! inner [`MmioTransport`] and intercepts exactly one method, [`Transport::set_status`],
//! inserting whatever intermediate step the transition table requires. Overriding
//! `set_status` rather than the `begin_init` default method is deliberate: it needs
//! no generic bounds, so it does not drag `bitflags` in as a direct dependency,
//! and it fixes *every* caller of `set_status` rather than one code path.
//!
//! It is also correct on QEMU — an extra `ACKNOWLEDGE` write before
//! `ACKNOWLEDGE|DRIVER` is a no-op there, since QEMU ORs bits anyway. There is no
//! platform `#[cfg]` here on purpose: one transport, spec-ordered on both machines.

use virtio_drivers::PhysAddr;
use virtio_drivers::transport::mmio::MmioTransport;
use virtio_drivers::transport::{DeviceStatus, DeviceType, InterruptStatus, Transport};

/// A [`MmioTransport`] whose status writes always follow virtio 1.0 §3.1.1 one
/// step at a time. See the module header.
///
/// The lifetime is `MmioTransport`'s own: as of virtio-drivers 0.13 it borrows
/// the MMIO region rather than owning a raw pointer to it.
#[derive(Debug)]
pub struct SteppedMmioTransport<'a>(MmioTransport<'a>);

impl<'a> SteppedMmioTransport<'a> {
    /// Wrap an already-constructed transport.
    #[must_use]
    pub const fn new(inner: MmioTransport<'a>) -> Self {
        Self(inner)
    }

    /// The wrapped transport.
    #[must_use]
    pub const fn inner(&self) -> &MmioTransport<'a> {
        &self.0
    }
}

impl Transport for SteppedMmioTransport<'_> {
    /// The one method that is not a plain delegation.
    ///
    /// Walks from the current status to `status` through the intermediate values
    /// Firecracker's transition table requires, skipping any that are already set.
    /// Reset (`empty()`) and `FAILED` are passed straight through — both are
    /// accepted from any state.
    fn set_status(&mut self, status: DeviceStatus) {
        // Reset and failure are valid from any state; do not decompose them.
        if status.is_empty() || status.contains(DeviceStatus::FAILED) {
            self.0.set_status(status);
            return;
        }

        // The ordered milestones of virtio 1.0 §3.1.1. Emit each one that the
        // caller's target includes and the device has not reached yet, in order.
        const STEPS: [DeviceStatus; 4] = [
            DeviceStatus::ACKNOWLEDGE,
            DeviceStatus::DRIVER,
            DeviceStatus::FEATURES_OK,
            DeviceStatus::DRIVER_OK,
        ];

        let mut current = self.0.get_status();
        for step in STEPS {
            if !status.contains(step) {
                // Target does not include this milestone, so nothing beyond it
                // can be legal either.
                break;
            }
            if current.contains(step) {
                continue; // already reached
            }
            current |= step;
            self.0.set_status(current);
        }

        // If the caller asked for something the step walk did not reproduce
        // exactly (bits outside STEPS), write it verbatim so behaviour is never
        // *worse* than the unwrapped transport.
        if current != status {
            self.0.set_status(status);
        }
    }

    // ---- plain delegation ------------------------------------------------------
    fn device_type(&self) -> DeviceType {
        self.0.device_type()
    }
    fn read_device_features(&mut self) -> u64 {
        self.0.read_device_features()
    }
    fn write_driver_features(&mut self, driver_features: u64) {
        self.0.write_driver_features(driver_features);
    }
    fn max_queue_size(&mut self, queue: u16) -> u32 {
        self.0.max_queue_size(queue)
    }
    fn notify(&mut self, queue: u16) {
        self.0.notify(queue);
    }
    fn get_status(&self) -> DeviceStatus {
        self.0.get_status()
    }
    fn set_guest_page_size(&mut self, guest_page_size: u32) {
        self.0.set_guest_page_size(guest_page_size);
    }
    fn requires_legacy_layout(&self) -> bool {
        self.0.requires_legacy_layout()
    }
    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: PhysAddr,
        driver_area: PhysAddr,
        device_area: PhysAddr,
    ) {
        self.0.queue_set(queue, size, descriptors, driver_area, device_area);
    }
    fn queue_unset(&mut self, queue: u16) {
        self.0.queue_unset(queue);
    }
    fn queue_used(&mut self, queue: u16) -> bool {
        self.0.queue_used(queue)
    }
    fn ack_interrupt(&mut self) -> InterruptStatus {
        self.0.ack_interrupt()
    }
    fn read_config_generation(&self) -> u32 {
        self.0.read_config_generation()
    }
    fn read_config_space<T: zerocopy::FromBytes + zerocopy::IntoBytes>(
        &self,
        offset: usize,
    ) -> virtio_drivers::Result<T> {
        self.0.read_config_space(offset)
    }
    fn write_config_space<T: zerocopy::IntoBytes + zerocopy::Immutable>(
        &mut self,
        offset: usize,
        value: T,
    ) -> virtio_drivers::Result<()> {
        self.0.write_config_space(offset, value)
    }
}

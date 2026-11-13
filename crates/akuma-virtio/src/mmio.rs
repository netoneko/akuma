//! The virtio-mmio transport: one slot's register window, the init handshake,
//! queue setup, notify and config reads.
//!
//! The decisions are `akuma-virtqueue`'s (register map, status order, feature
//! negotiation); this file performs them through an [`MmioWindow`], so its one
//! `unsafe` is naming the slot as a device window.
//!
//! The status register is written one milestone per write ([`status::through`]):
//! Firecracker discards any write that is not exactly the next value of its
//! transition table, QEMU ORs bits and does not care. This replaced the
//! `SteppedMmioTransport` wrapper around `virtio-drivers`, which existed only to
//! get that ordering out of a crate that wrote `ACKNOWLEDGE|DRIVER` in one go.

use core::sync::atomic::{Ordering, fence};

use akuma_primitives::mmio::MmioWindow;
use akuma_virtqueue::mmio as reg;
use akuma_virtqueue::{features, status};

/// Why a transport could not be brought up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The device does not offer `VERSION_1`.
    NotModern,
    /// The device cleared `FEATURES_OK`: it refused the feature set.
    FeaturesRefused,
    /// The queue is absent, smaller than asked for, or already live.
    Queue,
}

impl_display!(Error {
    NotModern => "device is not virtio 1.x",
    FeaturesRefused => "device refused the negotiated features",
    Queue => "virtqueue unavailable",
});

/// One virtio-mmio slot, known to hold a version-2 device.
#[derive(Debug)]
pub struct Transport {
    win: MmioWindow,
    slot: usize,
}

impl Transport {
    /// The device at virtio-mmio slot `slot`, if the slot shows the magic and
    /// version 2 and holds a device (an empty slot reads device id 0).
    #[must_use]
    pub fn at_slot(slot: usize) -> Option<Self> {
        if slot >= crate::probe::num_slots() {
            return None;
        }
        let base = crate::probe::slot_addr(slot);
        let len = akuma_primitives::addr::virtio_stride();
        if base == 0 || len < reg::CONFIG + 8 {
            return None;
        }
        // SAFETY: `base` is slot `slot` of the virtio-mmio window the kernel
        // mapped as device memory before any driver runs (`set_virtio_window`),
        // `len` is the slot stride the same call recorded, and the base is page-
        // or stride-aligned, so every naturally aligned register access inside it
        // reaches only this slot's device.
        let win = unsafe { MmioWindow::new(base, len) };
        let t = Self { win, slot };
        (reg::is_modern(t.r(reg::MAGIC_VALUE), t.r(reg::VERSION)) && t.device_id() != 0).then_some(t)
    }

    #[must_use]
    pub const fn slot(&self) -> usize {
        self.slot
    }

    /// The slot's register base (the address the IRQ path acknowledges through).
    #[must_use]
    pub const fn base(&self) -> usize {
        self.win.base()
    }

    #[must_use]
    pub fn device_id(&self) -> u32 {
        self.r(reg::DEVICE_ID)
    }

    fn r(&self, off: usize) -> u32 {
        self.win.read::<u32>(off)
    }

    fn w(&self, off: usize, v: u32) {
        self.win.write::<u32>(off, v);
    }

    /// Reset the device, walk the handshake to `FEATURES_OK`, and negotiate
    /// `wanted` (plus `VERSION_1`). Returns the accepted feature word. Queues are
    /// set up after this and before [`Self::driver_ok`].
    pub fn begin(&mut self, wanted: u64) -> Result<u64, Error> {
        self.w(reg::STATUS, 0);
        // A device may take a moment to finish a reset; the register reads 0 once it has.
        for _ in 0..1_000_000 {
            if self.r(reg::STATUS) == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        self.w(reg::STATUS, status::through(status::ACKNOWLEDGE));
        self.w(reg::STATUS, status::through(status::DRIVER));

        let offered = self.read_features();
        let accepted = match features::negotiate(offered, wanted) {
            Ok(f) => f,
            Err(features::Refused::NotModern) => {
                self.fail();
                return Err(Error::NotModern);
            }
        };
        #[allow(clippy::cast_possible_truncation)]
        for (sel, word) in [(0u32, accepted as u32), (1, (accepted >> 32) as u32)] {
            self.w(reg::DRIVER_FEATURES_SEL, sel);
            self.w(reg::DRIVER_FEATURES, word);
        }
        self.w(reg::STATUS, status::through(status::FEATURES_OK));
        if self.r(reg::STATUS) & status::FEATURES_OK == 0 {
            self.fail();
            return Err(Error::FeaturesRefused);
        }
        Ok(accepted)
    }

    fn read_features(&self) -> u64 {
        let mut f = 0u64;
        for sel in [1u32, 0] {
            self.w(reg::DEVICE_FEATURES_SEL, sel);
            f = f << 32 | u64::from(self.r(reg::DEVICE_FEATURES));
        }
        f
    }

    /// Give queue `q` its `size` entries and the device addresses of its three
    /// areas, and mark it ready.
    pub fn setup_queue(&mut self, q: u16, size: u16, desc: u64, avail: u64, used: u64) -> Result<(), Error> {
        self.w(reg::QUEUE_SEL, u32::from(q));
        let max = self.r(reg::QUEUE_NUM_MAX);
        if max == 0 || u32::from(size) > max || self.r(reg::QUEUE_READY) != 0 {
            return Err(Error::Queue);
        }
        self.w(reg::QUEUE_NUM, u32::from(size));
        #[allow(clippy::cast_possible_truncation)]
        for (lo, addr) in [(reg::QUEUE_DESC_LOW, desc), (reg::QUEUE_DRIVER_LOW, avail), (reg::QUEUE_DEVICE_LOW, used)] {
            self.w(lo, addr as u32);
            self.w(lo + 4, (addr >> 32) as u32);
        }
        // The rings' zeroing and the addresses before the device may use them.
        fence(Ordering::SeqCst);
        self.w(reg::QUEUE_READY, 1);
        Ok(())
    }

    /// The last handshake step: the device is live.
    pub fn driver_ok(&mut self) {
        self.w(reg::STATUS, status::through(status::DRIVER_OK));
    }

    fn fail(&self) {
        let s = self.r(reg::STATUS);
        self.w(reg::STATUS, s | status::FAILED);
    }

    /// Tell the device queue `q` has new buffers.
    pub fn notify(&self, q: u16) {
        self.w(reg::QUEUE_NOTIFY, u32::from(q));
    }

    /// One byte of device config space.
    #[must_use]
    pub fn config_u8(&self, off: usize) -> u8 {
        self.win.read::<u8>(reg::CONFIG + off)
    }

    /// A `u64` of device config space, read as two halves under the config
    /// generation so a concurrent device update cannot tear it.
    #[must_use]
    pub fn config_u64(&self, off: usize) -> u64 {
        loop {
            let g = self.r(reg::CONFIG_GENERATION);
            let lo = self.r(reg::CONFIG + off);
            let hi = self.r(reg::CONFIG + off + 4);
            if self.r(reg::CONFIG_GENERATION) == g {
                return u64::from(hi) << 32 | u64::from(lo);
            }
        }
    }
}

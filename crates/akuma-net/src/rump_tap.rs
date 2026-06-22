//! Raw L2 packet path for the kernel `rump` feature.
//!
//! Binds a **second** virtio-net device (NIC1) and exposes raw Ethernet
//! frame send/recv, completely bypassing smoltcp. The kernel surfaces this as a
//! `/dev/net/tap0` char device whose `read()`/`write()` move whole frames, so a
//! userspace rump kernel's stock Linux `virtif` backend can drive the NetBSD
//! TCP/IP stack over it.
//!
//! This module is the **hardware** half: it implements [`akuma_rump::RawNic`]
//! over virtio-drivers' `VirtIONetRaw` (real DMA via [`NetHal`]) and owns the
//! global instance. The **device-independent** orchestration — NIC selection,
//! the RX two-phase state machine, the malformed-length bounds guard — lives in
//! the `akuma-rump` crate, where it is unit-tested on the host with a mock NIC.
//!
//! NIC0 stays owned by smoltcp (the native stack); `init()` claims the second
//! virtio-net (the plan's §4 option A — dedicated second NIC). When no second
//! NIC is present (the default QEMU command line), `init()` returns `Err` and
//! the tap device never becomes ready; `/dev/net/tap0` then returns `ENODEV`.

use core::sync::atomic::{AtomicBool, Ordering};
use spinning_top::Spinlock;
use virtio_drivers::device::net::VirtIONetRaw;
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};
use alloc::vec::Vec;
use crate::hal::NetHal;
use akuma_rump::{NicError, RawNic, TapNic};

const VIRTIO_MMIO_DEVICE_ID_OFFSET: usize = 0x008;

/// The real raw NIC: a virtio-net device driven without buffer management.
/// Wraps the `unsafe` `VirtIONetRaw` calls in the safe [`RawNic`] trait so the
/// `akuma-rump` orchestration (and its host tests) need no virtio knowledge.
struct VirtioRawNic {
    inner: VirtIONetRaw<NetHal, MmioTransport, 16>,
}

impl VirtioRawNic {
    fn mac(&self) -> [u8; 6] {
        self.inner.mac_address()
    }
}

impl RawNic for VirtioRawNic {
    fn receive_begin(&mut self, buf: &mut [u8]) -> Result<u16, NicError> {
        unsafe { self.inner.receive_begin(buf) }.map_err(|_| NicError)
    }
    fn poll_receive(&mut self) -> bool {
        self.inner.poll_receive().is_some()
    }
    fn receive_complete(&mut self, token: u16, buf: &mut [u8]) -> Result<(usize, usize), NicError> {
        unsafe { self.inner.receive_complete(token, buf) }.map_err(|_| NicError)
    }
    fn send(&mut self, frame: &[u8]) -> Result<(), NicError> {
        self.inner.send(frame).map_err(|_| NicError)
    }
}

static TAP: Spinlock<Option<TapNic<VirtioRawNic>>> = Spinlock::new(None);
static READY: AtomicBool = AtomicBool::new(false);

/// Bind NIC1 to the tap path.
///
/// Probes the virtio-mmio slots for their device ids, then asks `akuma-rump`
/// which address is the **second** virtio-net (the first is smoltcp's NIC0).
/// Returns the NIC1 MAC on success, or `Err` if no second virtio-net exists.
pub fn init(mmio_addrs: &[usize]) -> Result<[u8; 6], &'static str> {
    // Probe device ids (hardware-bound) and let akuma-rump pick the slot.
    let slots: Vec<(usize, u32)> = mmio_addrs
        .iter()
        .map(|&addr| {
            let id = unsafe {
                core::ptr::read_volatile((addr + VIRTIO_MMIO_DEVICE_ID_OFFSET) as *const u32)
            };
            (addr, id)
        })
        .collect();

    let addr = akuma_rump::select_second_net_addr(&slots)
        .ok_or("tap: no second virtio-net (NIC1) device found")?;

    let header_ptr =
        core::ptr::NonNull::new(addr as *mut VirtIOHeader).ok_or("tap: bad mmio addr")?;
    let transport =
        unsafe { MmioTransport::new(header_ptr) }.map_err(|_| "tap: transport init failed")?;
    let inner = VirtIONetRaw::new(transport).map_err(|_| "tap: VirtIONetRaw init failed")?;

    let nic = VirtioRawNic { inner };
    let mac = nic.mac();
    *TAP.lock() = Some(TapNic::new(nic));
    READY.store(true, Ordering::Release);
    Ok(mac)
}

/// Whether NIC1 was found and bound (i.e. `/dev/net/tap0` is usable).
#[must_use]
pub fn is_ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// Pull one received L2 frame into `buf`. `Some(len)` if a frame was available
/// (truncated to `buf.len()`), `None` if none ready (caller → `EAGAIN`).
pub fn read_frame(buf: &mut [u8]) -> Option<usize> {
    let mut guard = TAP.lock();
    guard.as_mut()?.read_frame(buf)
}

/// Transmit one raw L2 (Ethernet) frame. Returns the bytes accepted on success.
pub fn write_frame(frame: &[u8]) -> Result<usize, &'static str> {
    let mut guard = TAP.lock();
    let tap = guard.as_mut().ok_or("tap: not ready")?;
    tap.write_frame(frame).map_err(|_| "tap: send failed")
}

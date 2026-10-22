//! A virtio network device on the virtio-over-MMIO transport (version 2), as a pure
//! state machine over two traits: [`GuestMem`] (the guest's memory) and [`NetBackend`]
//! (where frames come from and go to).
//!
//! Layouts are the OASIS virtio 1.x specification's: the register block (4.2.2), the
//! split virtqueue (2.7), and the network device (5.1): device ID 1, `receiveq1` is
//! queue 0 and `transmitq1` queue 1, every packet is preceded by a 12-byte
//! `virtio_net_hdr` (`VIRTIO_F_VERSION_1` fixes the header at that size, with
//! `num_buffers` at its end), and the configuration space starts with the MAC.
//!
//! Offered: `VIRTIO_F_VERSION_1`, `VIRTIO_NET_F_MAC`. Not offered: checksum or
//! segmentation offload, mergeable receive buffers, a control queue, `STATUS`,
//! multi-queue, indirect descriptors, event index — so every transmitted frame is
//! complete, every received one fits a single buffer, and a queue above 1 reads
//! `QueueNumMax = 0`.
//!
//! The device never raises an interrupt line itself: [`Processed::interrupt`] says
//! whether the driver asked to be told, and the caller decides what a guest without
//! an interrupt route does about it (the Akuma guest polls).

use crate::virtio::{rd16, rd32, rd64, reg, status, GuestMem, Processed, Write, MAGIC, QUEUE_MAX, VENDOR_ID};

/// Device ID of a network device.
pub const DEVICE_ID_NET: u32 = 1;
/// `sizeof(struct virtio_net_hdr)` with `VIRTIO_F_VERSION_1`.
pub const HDR_LEN: usize = 12;
/// The largest frame a guest may send or be sent (Ethernet header + 1500).
pub const MAX_FRAME: usize = 1514;

const F_MAC: u64 = 1 << 5;
const F_VERSION_1: u64 = 1 << 32;
const OFFERED: u64 = F_MAC | F_VERSION_1;
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
const AVAIL_NO_INTERRUPT: u16 = 1;
const RX: usize = 0;
const TX: usize = 1;

/// The wire: what the device exchanges frames with.
pub trait NetBackend {
    /// A frame the guest sent (without the virtio header).
    fn transmit(&mut self, frame: &[u8]);
    /// Is a frame waiting for the guest?
    fn has_frame(&self) -> bool;
    /// Move the next frame for the guest into `dst` and return its length. A frame too
    /// big for `dst` is dropped, and `Some(0)` returned so the caller knows to ask again.
    fn receive(&mut self, dst: &mut [u8]) -> Option<usize>;
}

#[derive(Clone, Copy, Debug, Default)]
struct Queue {
    num: u16,
    ready: bool,
    desc: u64,
    avail: u64,
    used: u64,
    /// Next available-ring slot to consume (free-running, wraps at 2^16).
    last_avail: u16,
}

/// The device: register file, two queues, interrupt status.
#[derive(Clone, Copy, Debug)]
pub struct MmioNet {
    mac: [u8; 6],
    status: u32,
    dev_features_sel: u32,
    drv_features_sel: u32,
    drv_features: u64,
    queue_sel: u32,
    queues: [Queue; 2],
    interrupt_status: u32,
}

impl MmioNet {
    /// A device with hardware address `mac`, in its reset state.
    #[must_use]
    pub const fn new(mac: [u8; 6]) -> Self {
        const Q: Queue = Queue { num: 0, ready: false, desc: 0, avail: 0, used: 0, last_avail: 0 };
        Self {
            mac,
            status: 0,
            dev_features_sel: 0,
            drv_features_sel: 0,
            drv_features: 0,
            queue_sel: 0,
            queues: [Q; 2],
            interrupt_status: 0,
        }
    }

    fn reset(&mut self) {
        *self = Self::new(self.mac);
    }

    /// The interrupt line's level: any unacknowledged status bit.
    #[must_use]
    pub const fn irq_level(&self) -> bool {
        self.interrupt_status != 0
    }

    /// Has the driver finished initialization (`DRIVER_OK`, not asked to reset)?
    #[must_use]
    pub const fn driver_ok(&self) -> bool {
        self.status & status::DRIVER_OK != 0 && self.status & status::NEEDS_RESET == 0
    }

    fn sel(&self) -> Option<usize> {
        (self.queue_sel < 2).then_some(self.queue_sel as usize)
    }

    /// A read of `size` bytes (1, 2, 4 or 8) at `off` into the register block.
    #[must_use]
    pub fn read(&self, off: u64, size: u8) -> u64 {
        if off >= reg::CONFIG {
            let mut v = 0u64;
            for k in 0..u64::from(size.min(8)) {
                let at = (off - reg::CONFIG + k) as usize;
                v |= u64::from(self.mac.get(at).copied().unwrap_or(0)) << (8 * k);
            }
            return v;
        }
        if size != 4 || off & 3 != 0 {
            return 0;
        }
        let q = self.sel().map(|i| self.queues[i]);
        u64::from(match off {
            reg::MAGIC => MAGIC,
            reg::VERSION => 2,
            reg::DEVICE_ID => DEVICE_ID_NET,
            reg::VENDOR_ID => VENDOR_ID,
            reg::DEVICE_FEATURES => match self.dev_features_sel {
                0 => OFFERED as u32,
                1 => (OFFERED >> 32) as u32,
                _ => 0,
            },
            reg::QUEUE_NUM_MAX => if q.is_some() { u32::from(QUEUE_MAX) } else { 0 },
            reg::QUEUE_READY => u32::from(q.is_some_and(|q| q.ready)),
            reg::INTERRUPT_STATUS => self.interrupt_status,
            reg::STATUS => self.status,
            _ => 0,
        })
    }

    /// A write of `size` bytes of `val` at `off`.
    pub fn write(&mut self, off: u64, size: u8, val: u64) -> Write {
        // The configuration space is read-only here.
        if off >= reg::CONFIG || size != 4 || off & 3 != 0 {
            return Write::Done;
        }
        let v = val as u32;
        let sel = self.sel();
        let lo = |old: u64| (old & !0xFFFF_FFFF) | u64::from(v);
        let hi = |old: u64| (old & 0xFFFF_FFFF) | (u64::from(v) << 32);
        match off {
            reg::DEVICE_FEATURES_SEL => self.dev_features_sel = v,
            reg::DRIVER_FEATURES_SEL => self.drv_features_sel = v,
            reg::DRIVER_FEATURES => match self.drv_features_sel {
                0 => self.drv_features = lo(self.drv_features),
                1 => self.drv_features = hi(self.drv_features),
                _ => {}
            },
            reg::QUEUE_SEL => self.queue_sel = v,
            reg::QUEUE_NUM => {
                if let Some(i) = sel {
                    let n = v as u16;
                    self.queues[i].num = if v <= u32::from(QUEUE_MAX) && n.is_power_of_two() { n } else { 0 };
                }
            }
            reg::QUEUE_READY => {
                if let Some(i) = sel {
                    let q = &mut self.queues[i];
                    q.ready = v & 1 != 0 && q.num != 0;
                    if q.ready {
                        q.last_avail = 0;
                    }
                }
            }
            reg::QUEUE_DESC_LOW => self.set_queue(sel, |q| q.desc = lo(q.desc)),
            reg::QUEUE_DESC_HIGH => self.set_queue(sel, |q| q.desc = hi(q.desc)),
            reg::QUEUE_DRIVER_LOW => self.set_queue(sel, |q| q.avail = lo(q.avail)),
            reg::QUEUE_DRIVER_HIGH => self.set_queue(sel, |q| q.avail = hi(q.avail)),
            reg::QUEUE_DEVICE_LOW => self.set_queue(sel, |q| q.used = lo(q.used)),
            reg::QUEUE_DEVICE_HIGH => self.set_queue(sel, |q| q.used = hi(q.used)),
            reg::QUEUE_NOTIFY => return Write::Notify(v as u16),
            reg::INTERRUPT_ACK => self.interrupt_status &= !v,
            reg::STATUS => {
                if v == 0 {
                    self.reset();
                    return Write::Reset;
                }
                self.status = v;
                if v & status::FEATURES_OK != 0 && (self.drv_features & F_VERSION_1 == 0 || self.drv_features & !OFFERED != 0) {
                    self.status &= !status::FEATURES_OK;
                }
            }
            _ => {}
        }
        Write::Done
    }

    fn set_queue(&mut self, sel: Option<usize>, f: impl FnOnce(&mut Queue)) {
        if let Some(i) = sel {
            f(&mut self.queues[i]);
        }
    }

    fn usable(&self, qi: usize) -> bool {
        self.queues[qi].ready && self.driver_ok()
    }

    fn broken(&mut self, out: Processed) -> Processed {
        self.status |= status::NEEDS_RESET;
        self.interrupt_status |= 2;
        Processed { completed: out.completed, interrupt: true }
    }

    /// The head of the next available buffer on queue `qi`, without consuming it.
    /// `Err` is a ring the driver has corrupted.
    fn peek(&self, qi: usize, mem: &dyn GuestMem) -> Result<Option<u16>, ()> {
        let q = self.queues[qi];
        let idx = rd16(mem, q.avail + 2).ok_or(())?;
        if idx.wrapping_sub(q.last_avail) > q.num {
            return Err(());
        }
        if idx == q.last_avail {
            return Ok(None);
        }
        let head = rd16(mem, q.avail + 4 + 2 * u64::from(q.last_avail % q.num)).ok_or(())?;
        if head >= q.num {
            return Err(());
        }
        Ok(Some(head))
    }

    /// Put `head` on the used ring with `len` bytes written, and consume it.
    fn complete(&mut self, qi: usize, mem: &dyn GuestMem, head: u16, len: u32) -> bool {
        let q = self.queues[qi];
        let Some(used_idx) = rd16(mem, q.used + 2) else { return false };
        let mut elem = [0u8; 8];
        elem[..4].copy_from_slice(&u32::from(head).to_le_bytes());
        elem[4..].copy_from_slice(&len.to_le_bytes());
        if !mem.write(q.used + 4 + 8 * u64::from(used_idx % q.num), &elem) {
            return false;
        }
        // The index follows the element: a driver that sees it sees the element.
        if !mem.write(q.used + 2, &used_idx.wrapping_add(1).to_le_bytes()) {
            return false;
        }
        self.queues[qi].last_avail = q.last_avail.wrapping_add(1);
        true
    }

    /// Visit the descriptors of the chain at `head`: `(address, length, device-writable)`.
    /// False if the chain is malformed (out-of-range index, a loop).
    fn walk(&self, qi: usize, mem: &dyn GuestMem, head: u16, mut f: impl FnMut(u64, u32, bool)) -> bool {
        let q = self.queues[qi];
        let (mut cur, mut hops) = (head, 0u32);
        loop {
            hops += 1;
            if hops > u32::from(q.num) || cur >= q.num {
                return false;
            }
            let base = q.desc + 16 * u64::from(cur);
            let (Some(addr), Some(len), Some(flags), Some(next)) =
                (rd64(mem, base), rd32(mem, base + 8), rd16(mem, base + 12), rd16(mem, base + 14))
            else {
                return false;
            };
            f(addr, len, flags & DESC_WRITE != 0);
            if flags & DESC_NEXT == 0 {
                return true;
            }
            cur = next;
        }
    }

    fn finish(&mut self, mem: &dyn GuestMem, qi: usize, out: &mut Processed) {
        let flags = rd16(mem, self.queues[qi].avail).unwrap_or(0);
        if out.completed > 0 && flags & AVAIL_NO_INTERRUPT == 0 {
            self.interrupt_status |= 1;
            out.interrupt = true;
        }
    }

    /// Hand every frame the driver has queued for transmission to `backend`. `bounce`
    /// holds one frame plus its header (`HDR_LEN + MAX_FRAME` bytes at least).
    pub fn process_tx(&mut self, mem: &dyn GuestMem, backend: &mut dyn NetBackend, bounce: &mut [u8]) -> Processed {
        let mut out = Processed::default();
        if !self.usable(TX) {
            return out;
        }
        loop {
            let head = match self.peek(TX, mem) {
                Ok(Some(h)) => h,
                Ok(None) => break,
                Err(()) => return self.broken(out),
            };
            // Gather the device-readable part of the chain: header, then the frame.
            let mut total = 0usize;
            let mut fits = true;
            let ok = self.walk(TX, mem, head, |addr, len, write| {
                if write || !fits {
                    return;
                }
                let n = len as usize;
                if total + n > bounce.len() || !mem.read(addr, &mut bounce[total..total + n]) {
                    fits = false;
                    return;
                }
                total += n;
            });
            if !ok {
                return self.broken(out);
            }
            // A frame that is too long, unreadable or headerless is dropped, not retried:
            // the buffer is still returned to the driver.
            if fits && total > HDR_LEN && total - HDR_LEN <= MAX_FRAME {
                backend.transmit(&bounce[HDR_LEN..total]);
            }
            if !self.complete(TX, mem, head, 0) {
                return self.broken(out);
            }
            out.completed += 1;
        }
        self.finish(mem, TX, &mut out);
        out
    }

    /// Fill the driver's posted receive buffers from `backend` while it has frames.
    /// `bounce` as for [`MmioNet::process_tx`].
    pub fn deliver_rx(&mut self, mem: &dyn GuestMem, backend: &mut dyn NetBackend, bounce: &mut [u8]) -> Processed {
        let mut out = Processed::default();
        if !self.usable(RX) {
            return out;
        }
        while backend.has_frame() {
            let head = match self.peek(RX, mem) {
                Ok(Some(h)) => h,
                // No buffer posted: the frame waits.
                Ok(None) => break,
                Err(()) => return self.broken(out),
            };
            // Room in the chain's device-writable descriptors, after the header.
            let mut room = 0usize;
            let mut readable = false;
            if !self.walk(RX, mem, head, |_, len, write| {
                if write {
                    room += len as usize;
                } else {
                    readable = true;
                }
            }) {
                return self.broken(out);
            }
            if readable || room < HDR_LEN {
                // Not a receive buffer at all: give it back empty.
                if !self.complete(RX, mem, head, 0) {
                    return self.broken(out);
                }
                out.completed += 1;
                continue;
            }
            let cap = (room - HDR_LEN).min(MAX_FRAME).min(bounce.len().saturating_sub(HDR_LEN));
            let Some(n) = backend.receive(&mut bounce[HDR_LEN..HDR_LEN + cap]) else { break };
            if n == 0 {
                // Dropped as too big for this buffer; the buffer stays posted.
                continue;
            }
            bounce[..HDR_LEN].fill(0);
            // `num_buffers`: one, the whole frame is in this buffer.
            bounce[10..12].copy_from_slice(&1u16.to_le_bytes());
            let total = HDR_LEN + n;
            let (mut at, mut ok) = (0usize, true);
            let walked = self.walk(RX, mem, head, |addr, len, _| {
                let take = (len as usize).min(total - at);
                if take > 0 {
                    ok &= mem.write(addr, &bounce[at..at + take]);
                    at += take;
                }
            });
            if !walked || !ok || !self.complete(RX, mem, head, total as u32) {
                return self.broken(out);
            }
            out.completed += 1;
        }
        self.finish(mem, RX, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use core::cell::RefCell;
    use std::collections::VecDeque;
    use std::vec;
    use std::vec::Vec;

    struct Mem(RefCell<Vec<u8>>);
    impl Mem {
        fn new(n: usize) -> Self {
            Self(RefCell::new(vec![0; n]))
        }
        fn put(&self, gpa: u64, b: &[u8]) {
            assert!(self.write(gpa, b));
        }
        fn get(&self, gpa: u64, n: usize) -> Vec<u8> {
            let mut v = vec![0; n];
            assert!(self.read(gpa, &mut v));
            v
        }
    }
    impl GuestMem for Mem {
        fn read(&self, gpa: u64, dst: &mut [u8]) -> bool {
            let m = self.0.borrow();
            let a = gpa as usize;
            m.get(a..a + dst.len()).is_some_and(|s| {
                dst.copy_from_slice(s);
                true
            })
        }
        fn write(&self, gpa: u64, src: &[u8]) -> bool {
            let mut m = self.0.borrow_mut();
            let a = gpa as usize;
            m.get_mut(a..a + src.len()).is_some_and(|s| {
                s.copy_from_slice(src);
                true
            })
        }
    }

    #[derive(Default)]
    struct Wire {
        sent: Vec<Vec<u8>>,
        to_guest: VecDeque<Vec<u8>>,
    }
    impl NetBackend for Wire {
        fn transmit(&mut self, frame: &[u8]) {
            self.sent.push(frame.to_vec());
        }
        fn has_frame(&self) -> bool {
            !self.to_guest.is_empty()
        }
        fn receive(&mut self, dst: &mut [u8]) -> Option<usize> {
            let f = self.to_guest.pop_front()?;
            if f.len() > dst.len() {
                return Some(0);
            }
            dst[..f.len()].copy_from_slice(&f);
            Some(f.len())
        }
    }

    const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const NUM: u16 = 8;
    // Queue 0 (receive) and queue 1 (transmit) rings, a page apart.
    const RXQ: [u64; 3] = [0x1000, 0x2000, 0x3000];
    const TXQ: [u64; 3] = [0x4000, 0x5000, 0x6000];
    const BUF: u64 = 0x8000;

    fn bring_up() -> MmioNet {
        let mut d = MmioNet::new(MAC);
        assert_eq!(d.read(reg::MAGIC, 4), u64::from(MAGIC));
        assert_eq!(d.read(reg::VERSION, 4), 2);
        assert_eq!(d.read(reg::DEVICE_ID, 4), 1);
        d.write(reg::STATUS, 4, 1);
        d.write(reg::STATUS, 4, 3);
        d.write(reg::DRIVER_FEATURES_SEL, 4, 0);
        d.write(reg::DRIVER_FEATURES, 4, F_MAC);
        d.write(reg::DRIVER_FEATURES_SEL, 4, 1);
        d.write(reg::DRIVER_FEATURES, 4, 1);
        d.write(reg::STATUS, 4, 3 | u64::from(status::FEATURES_OK));
        assert_ne!(d.read(reg::STATUS, 4) & u64::from(status::FEATURES_OK), 0);
        for (sel, ring) in [(0, RXQ), (1, TXQ)] {
            d.write(reg::QUEUE_SEL, 4, sel);
            assert_eq!(d.read(reg::QUEUE_NUM_MAX, 4), u64::from(QUEUE_MAX));
            d.write(reg::QUEUE_NUM, 4, u64::from(NUM));
            for (lo, hi, a) in [
                (reg::QUEUE_DESC_LOW, reg::QUEUE_DESC_HIGH, ring[0]),
                (reg::QUEUE_DRIVER_LOW, reg::QUEUE_DRIVER_HIGH, ring[1]),
                (reg::QUEUE_DEVICE_LOW, reg::QUEUE_DEVICE_HIGH, ring[2]),
            ] {
                d.write(lo, 4, a & 0xFFFF_FFFF);
                d.write(hi, 4, a >> 32);
            }
            d.write(reg::QUEUE_READY, 4, 1);
            assert_eq!(d.read(reg::QUEUE_READY, 4), 1);
        }
        d.write(reg::STATUS, 4, 7 | u64::from(status::FEATURES_OK));
        assert!(d.driver_ok());
        d
    }

    fn desc(m: &Mem, ring: [u64; 3], i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&addr.to_le_bytes());
        b[8..12].copy_from_slice(&len.to_le_bytes());
        b[12..14].copy_from_slice(&flags.to_le_bytes());
        b[14..].copy_from_slice(&next.to_le_bytes());
        m.put(ring[0] + 16 * u64::from(i), &b);
    }

    fn make_available(m: &Mem, ring: [u64; 3], slot: u16, head: u16) {
        m.put(ring[1] + 4 + 2 * u64::from(slot % NUM), &head.to_le_bytes());
        m.put(ring[1] + 2, &(slot + 1).to_le_bytes());
    }

    fn used_idx(m: &Mem, ring: [u64; 3]) -> u16 {
        u16::from_le_bytes(m.get(ring[2] + 2, 2).try_into().unwrap())
    }

    fn bounce() -> Vec<u8> {
        vec![0; HDR_LEN + MAX_FRAME]
    }

    #[test]
    fn identity_features_and_mac() {
        let mut d = MmioNet::new(MAC);
        assert_eq!(d.read(reg::DEVICE_ID, 4), 1);
        d.write(reg::DEVICE_FEATURES_SEL, 4, 0);
        assert_eq!(d.read(reg::DEVICE_FEATURES, 4), F_MAC);
        d.write(reg::DEVICE_FEATURES_SEL, 4, 1);
        assert_eq!(d.read(reg::DEVICE_FEATURES, 4), 1, "VERSION_1");
        for (i, b) in MAC.iter().enumerate() {
            assert_eq!(d.read(reg::CONFIG + i as u64, 1), u64::from(*b));
        }
        // A 32-bit read of the first four bytes, little-endian.
        assert_eq!(d.read(reg::CONFIG, 4), u64::from(u32::from_le_bytes([MAC[0], MAC[1], MAC[2], MAC[3]])));
        assert_eq!(d.read(reg::CONFIG + 6, 2), 0, "no status field: it was not offered");
    }

    #[test]
    fn exactly_two_queues_exist() {
        let mut d = MmioNet::new(MAC);
        for (sel, max) in [(0, u64::from(QUEUE_MAX)), (1, u64::from(QUEUE_MAX)), (2, 0), (3, 0)] {
            d.write(reg::QUEUE_SEL, 4, sel);
            assert_eq!(d.read(reg::QUEUE_NUM_MAX, 4), max, "queue {sel}");
        }
        // Writes to a nonexistent queue change nothing and do not panic.
        d.write(reg::QUEUE_SEL, 4, 2);
        d.write(reg::QUEUE_NUM, 4, 8);
        d.write(reg::QUEUE_READY, 4, 1);
        assert_eq!(d.read(reg::QUEUE_READY, 4), 0);
    }

    #[test]
    fn features_ok_needs_version_1_and_nothing_unoffered() {
        let mut d = MmioNet::new(MAC);
        d.write(reg::DRIVER_FEATURES_SEL, 4, 0);
        d.write(reg::DRIVER_FEATURES, 4, F_MAC | 1); // bit 0: checksum offload, not offered
        d.write(reg::DRIVER_FEATURES_SEL, 4, 1);
        d.write(reg::DRIVER_FEATURES, 4, 1);
        d.write(reg::STATUS, 4, u64::from(status::FEATURES_OK));
        assert_eq!(d.read(reg::STATUS, 4) & u64::from(status::FEATURES_OK), 0);
    }

    #[test]
    fn a_transmitted_frame_reaches_the_backend_without_its_header() {
        let m = Mem::new(0x10000);
        let mut d = bring_up();
        let mut w = Wire::default();
        // Header and frame in two descriptors, as Linux builds them.
        let frame: Vec<u8> = (0..60).collect();
        m.put(BUF, &[0u8; HDR_LEN]);
        m.put(BUF + 0x100, &frame);
        desc(&m, TXQ, 0, BUF, HDR_LEN as u32, DESC_NEXT, 1);
        desc(&m, TXQ, 1, BUF + 0x100, 60, 0, 0);
        make_available(&m, TXQ, 0, 0);
        assert_eq!(d.write(reg::QUEUE_NOTIFY, 4, 1), Write::Notify(1));
        let out = d.process_tx(&m, &mut w, &mut bounce());
        assert_eq!(out, Processed { completed: 1, interrupt: true });
        assert_eq!(w.sent, vec![frame]);
        assert_eq!(used_idx(&m, TXQ), 1);
        assert_eq!(m.get(TXQ[2] + 4, 8), [0, 0, 0, 0, 0, 0, 0, 0], "head 0, nothing written");
        assert!(d.irq_level());
        d.write(reg::INTERRUPT_ACK, 4, 1);
        assert!(!d.irq_level());
    }

    #[test]
    fn header_and_frame_in_one_descriptor_work_too() {
        let m = Mem::new(0x10000);
        let mut d = bring_up();
        let mut w = Wire::default();
        let mut buf = vec![0u8; HDR_LEN];
        buf.extend_from_slice(&[7u8; 42]);
        m.put(BUF, &buf);
        desc(&m, TXQ, 3, BUF, buf.len() as u32, 0, 0);
        make_available(&m, TXQ, 0, 3);
        d.process_tx(&m, &mut w, &mut bounce());
        assert_eq!(w.sent, vec![vec![7u8; 42]]);
        assert_eq!(m.get(TXQ[2] + 4, 4), [3, 0, 0, 0]);
    }

    #[test]
    fn an_oversized_or_headerless_transmit_is_dropped_but_returned() {
        let m = Mem::new(0x10000);
        let mut d = bring_up();
        let mut w = Wire::default();
        // Only a header: no frame.
        m.put(BUF, &[0u8; HDR_LEN]);
        desc(&m, TXQ, 0, BUF, HDR_LEN as u32, 0, 0);
        // A frame one byte over the limit.
        desc(&m, TXQ, 1, BUF, (HDR_LEN + MAX_FRAME + 1) as u32, 0, 0);
        make_available(&m, TXQ, 0, 0);
        m.put(TXQ[1] + 4 + 2, &1u16.to_le_bytes());
        m.put(TXQ[1] + 2, &2u16.to_le_bytes());
        let out = d.process_tx(&m, &mut w, &mut bounce());
        assert_eq!(out.completed, 2);
        assert!(w.sent.is_empty());
        assert_eq!(used_idx(&m, TXQ), 2);
        assert!(d.driver_ok());
    }

    #[test]
    fn a_received_frame_lands_after_a_header_with_num_buffers_one() {
        let m = Mem::new(0x10000);
        let mut d = bring_up();
        let mut w = Wire::default();
        let frame: Vec<u8> = (100..160).collect();
        w.to_guest.push_back(frame.clone());
        // Nothing posted yet: the frame waits.
        assert_eq!(d.deliver_rx(&m, &mut w, &mut bounce()).completed, 0);
        assert!(w.has_frame());
        // One 2048-byte buffer, split over two descriptors to test the scatter.
        desc(&m, RXQ, 0, BUF, 20, DESC_WRITE | DESC_NEXT, 1);
        desc(&m, RXQ, 1, BUF + 0x100, 2028, DESC_WRITE, 0);
        make_available(&m, RXQ, 0, 0);
        let out = d.deliver_rx(&m, &mut w, &mut bounce());
        assert_eq!(out, Processed { completed: 1, interrupt: true });
        assert!(!w.has_frame());
        assert_eq!(u32::from_le_bytes(m.get(RXQ[2] + 8, 4).try_into().unwrap()), (HDR_LEN + 60) as u32);
        let mut got = m.get(BUF, 20);
        got.extend(m.get(BUF + 0x100, HDR_LEN + 60 - 20));
        assert_eq!(&got[..10], &[0; 10]);
        assert_eq!(u16::from_le_bytes([got[10], got[11]]), 1, "num_buffers");
        assert_eq!(&got[HDR_LEN..], &frame[..]);
    }

    #[test]
    fn a_frame_too_big_for_the_buffer_is_dropped_and_the_buffer_kept() {
        let m = Mem::new(0x10000);
        let mut d = bring_up();
        let mut w = Wire::default();
        w.to_guest.push_back(vec![1u8; 200]);
        w.to_guest.push_back(vec![2u8; 30]);
        desc(&m, RXQ, 0, BUF, 100, DESC_WRITE, 0); // 100 - 12 = 88 bytes of room
        make_available(&m, RXQ, 0, 0);
        let out = d.deliver_rx(&m, &mut w, &mut bounce());
        // The first frame did not fit and went; the second did.
        assert_eq!(out.completed, 1);
        assert_eq!(m.get(BUF + HDR_LEN as u64, 30), vec![2u8; 30]);
        assert!(!w.has_frame());
    }

    #[test]
    fn rx_stops_when_the_buffers_run_out_and_resumes_when_more_are_posted() {
        let m = Mem::new(0x10000);
        let mut d = bring_up();
        let mut w = Wire::default();
        for k in 0..3u8 {
            w.to_guest.push_back(vec![k; 20]);
        }
        desc(&m, RXQ, 0, BUF, 2048, DESC_WRITE, 0);
        desc(&m, RXQ, 1, BUF + 0x800, 2048, DESC_WRITE, 0);
        make_available(&m, RXQ, 0, 0);
        m.put(RXQ[1] + 4 + 2, &1u16.to_le_bytes());
        m.put(RXQ[1] + 2, &2u16.to_le_bytes());
        assert_eq!(d.deliver_rx(&m, &mut w, &mut bounce()).completed, 2);
        assert_eq!(w.to_guest.len(), 1, "the third waits for a buffer");
        desc(&m, RXQ, 2, BUF + 0x1000, 2048, DESC_WRITE, 0);
        m.put(RXQ[1] + 4 + 4, &2u16.to_le_bytes());
        m.put(RXQ[1] + 2, &3u16.to_le_bytes());
        assert_eq!(d.deliver_rx(&m, &mut w, &mut bounce()).completed, 1);
        assert!(!w.has_frame());
        assert_eq!(used_idx(&m, RXQ), 3);
    }

    #[test]
    fn no_interrupt_flag_suppresses_the_line() {
        let m = Mem::new(0x10000);
        let mut d = bring_up();
        let mut w = Wire::default();
        w.to_guest.push_back(vec![9u8; 20]);
        m.put(RXQ[1], &AVAIL_NO_INTERRUPT.to_le_bytes());
        desc(&m, RXQ, 0, BUF, 2048, DESC_WRITE, 0);
        make_available(&m, RXQ, 0, 0);
        let out = d.deliver_rx(&m, &mut w, &mut bounce());
        assert_eq!(out, Processed { completed: 1, interrupt: false });
        assert!(!d.irq_level());
    }

    #[test]
    fn nothing_is_served_before_driver_ok_and_a_reset_forgets_the_queues() {
        let m = Mem::new(0x10000);
        let mut d = MmioNet::new(MAC);
        let mut w = Wire::default();
        w.to_guest.push_back(vec![1u8; 20]);
        assert_eq!(d.deliver_rx(&m, &mut w, &mut bounce()), Processed::default());
        assert_eq!(d.process_tx(&m, &mut w, &mut bounce()), Processed::default());
        let mut d = bring_up();
        assert_eq!(d.write(reg::STATUS, 4, 0), Write::Reset);
        d.write(reg::QUEUE_SEL, 4, 1);
        assert_eq!(d.read(reg::QUEUE_READY, 4), 0);
        assert!(!d.driver_ok());
    }

    #[test]
    fn a_looping_chain_or_a_runaway_index_needs_a_reset_instead_of_spinning() {
        let m = Mem::new(0x10000);
        let mut d = bring_up();
        let mut w = Wire::default();
        // A TX chain that points at itself.
        desc(&m, TXQ, 0, BUF, 16, DESC_NEXT, 0);
        make_available(&m, TXQ, 0, 0);
        let out = d.process_tx(&m, &mut w, &mut bounce());
        assert!(out.interrupt);
        assert_ne!(d.read(reg::STATUS, 4) & u64::from(status::NEEDS_RESET), 0);
        // More outstanding buffers than the ring holds.
        let mut d = bring_up();
        m.put(TXQ[1] + 2, &(NUM + 1).to_le_bytes());
        let out = d.process_tx(&m, &mut w, &mut bounce());
        assert!(out.interrupt);
        assert!(!d.driver_ok());
    }

    /// The two halves together, the way a guest's stack drives them: a DHCP DISCOVER goes
    /// down the transmit queue and the gateway's OFFER comes up the receive queue.
    #[test]
    fn a_dhcp_discover_through_the_device_is_answered_through_the_device() {
        use crate::gateway::{build_udp, Gateway, GATEWAY_IP, GATEWAY_MAC, GUEST_IP, GUEST_MAC};
        let m = Mem::new(0x10000);
        let mut d = bring_up();
        let mut wire = Gateway::new();
        // A receive buffer is posted first, as a driver does.
        desc(&m, RXQ, 0, BUF + 0x2000, 2048, DESC_WRITE, 0);
        make_available(&m, RXQ, 0, 0);
        // The DISCOVER: BOOTP request, Ethernet/6, magic cookie, option 53 = 1.
        let mut body = vec![0u8; 240];
        body[0] = 1;
        body[1] = 1;
        body[2] = 6;
        body[4..8].copy_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
        body[28..34].copy_from_slice(&GUEST_MAC);
        body[236..240].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);
        body.extend_from_slice(&[53, 1, 1, 255]);
        let mut frame = vec![0u8; 600];
        let n = build_udp(&mut frame, GATEWAY_MAC, [0; 4], [255; 4], 68, 67, &body).unwrap();
        let mut out = vec![0u8; HDR_LEN];
        out.extend_from_slice(&frame[..n]);
        m.put(BUF, &out);
        desc(&m, TXQ, 0, BUF, out.len() as u32, 0, 0);
        make_available(&m, TXQ, 0, 0);
        let mut b = bounce();
        let tx = d.process_tx(&m, &mut wire, &mut b);
        assert_eq!(tx.completed, 1);
        assert_eq!(wire.stats.dhcp_offers, 1);
        let rx = d.deliver_rx(&m, &mut wire, &mut b);
        assert_eq!(rx.completed, 1);
        let len = u32::from_le_bytes(m.get(RXQ[2] + 8, 4).try_into().unwrap()) as usize;
        let got = m.get(BUF + 0x2000, len);
        let f = &got[HDR_LEN..];
        assert_eq!(&f[26..30], &GATEWAY_IP, "from the gateway");
        // Ethernet 14 + IP 20 + UDP 8, then BOOTP: op, ..., xid at 4, yiaddr at 16.
        let bootp = &f[42..];
        assert_eq!(bootp[0], 2);
        assert_eq!(&bootp[4..8], &[0xCA, 0xFE, 0xBA, 0xBE]);
        assert_eq!(&bootp[16..20], &GUEST_IP);
    }
}

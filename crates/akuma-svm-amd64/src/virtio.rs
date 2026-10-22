//! A virtio block device on the virtio-over-MMIO transport (version 2, "modern"),
//! as a pure state machine over two traits: [`GuestMem`] (the guest's physical
//! memory) and [`Disk`] (the backing store).
//!
//! Layouts are the OASIS virtio 1.x specification's: the MMIO register block
//! (section 4.2.2), the split virtqueue (2.7: descriptor table, available ring,
//! used ring), and the block device's request header, status byte and
//! configuration space (5.2). Nothing here knows about SVM, the VMCB or the host
//! kernel — the hypervisor decodes a faulting `MOV` ([`crate::mmio`]), calls
//! [`MmioBlk::read`]/[`MmioBlk::write`], and when a write returns
//! [`Write::Notify`] calls [`MmioBlk::process`].
//!
//! Offered: `VIRTIO_F_VERSION_1`, `VIRTIO_BLK_F_SEG_MAX`, `VIRTIO_BLK_F_FLUSH`.
//! Not offered: indirect descriptors, event index, packed rings, multi-queue —
//! a driver may not use what was not offered, and a queue other than 0 reads
//! `QueueNumMax = 0` (absent).

/// "virt", little-endian: the transport's magic value.
pub const MAGIC: u32 = 0x7472_6976;
/// Device ID of a block device.
pub const DEVICE_ID_BLOCK: u32 = 2;
/// Vendor ID reported ("AKUM").
pub const VENDOR_ID: u32 = 0x4D55_4B41;
/// Queue size offered (a power of two, at most 32768).
pub const QUEUE_MAX: u16 = 128;
/// Largest number of data segments the driver is told to build per request.
pub const SEG_MAX: u32 = 32;
/// Bytes per sector.
pub const SECTOR: u64 = 512;

/// Offsets in the MMIO register block (spec 4.2.2).
pub mod reg {
    /// MagicValue.
    pub const MAGIC: u64 = 0x000;
    /// Version.
    pub const VERSION: u64 = 0x004;
    /// DeviceID.
    pub const DEVICE_ID: u64 = 0x008;
    /// VendorID.
    pub const VENDOR_ID: u64 = 0x00C;
    /// DeviceFeatures (window selected by `DEVICE_FEATURES_SEL`).
    pub const DEVICE_FEATURES: u64 = 0x010;
    /// DeviceFeaturesSel.
    pub const DEVICE_FEATURES_SEL: u64 = 0x014;
    /// DriverFeatures.
    pub const DRIVER_FEATURES: u64 = 0x020;
    /// DriverFeaturesSel.
    pub const DRIVER_FEATURES_SEL: u64 = 0x024;
    /// QueueSel.
    pub const QUEUE_SEL: u64 = 0x030;
    /// QueueNumMax.
    pub const QUEUE_NUM_MAX: u64 = 0x034;
    /// QueueNum.
    pub const QUEUE_NUM: u64 = 0x038;
    /// QueueReady.
    pub const QUEUE_READY: u64 = 0x044;
    /// QueueNotify.
    pub const QUEUE_NOTIFY: u64 = 0x050;
    /// InterruptStatus.
    pub const INTERRUPT_STATUS: u64 = 0x060;
    /// InterruptACK.
    pub const INTERRUPT_ACK: u64 = 0x064;
    /// Status.
    pub const STATUS: u64 = 0x070;
    /// QueueDescLow / High.
    pub const QUEUE_DESC_LOW: u64 = 0x080;
    /// See [`QUEUE_DESC_LOW`].
    pub const QUEUE_DESC_HIGH: u64 = 0x084;
    /// QueueDriverLow (the available ring) / High.
    pub const QUEUE_DRIVER_LOW: u64 = 0x090;
    /// See [`QUEUE_DRIVER_LOW`].
    pub const QUEUE_DRIVER_HIGH: u64 = 0x094;
    /// QueueDeviceLow (the used ring) / High.
    pub const QUEUE_DEVICE_LOW: u64 = 0x0A0;
    /// See [`QUEUE_DEVICE_LOW`].
    pub const QUEUE_DEVICE_HIGH: u64 = 0x0A4;
    /// ConfigGeneration.
    pub const CONFIG_GENERATION: u64 = 0x0FC;
    /// Start of the device-specific configuration space.
    pub const CONFIG: u64 = 0x100;
}

/// Device status bits (spec 2.1).
pub mod status {
    /// ACKNOWLEDGE.
    pub const ACKNOWLEDGE: u32 = 1;
    /// DRIVER.
    pub const DRIVER: u32 = 2;
    /// DRIVER_OK.
    pub const DRIVER_OK: u32 = 4;
    /// FEATURES_OK.
    pub const FEATURES_OK: u32 = 8;
    /// DEVICE_NEEDS_RESET.
    pub const NEEDS_RESET: u32 = 0x40;
}

/// Feature bits, in the 64-bit feature space.
mod feat {
    pub const SEG_MAX: u64 = 1 << 2;
    pub const FLUSH: u64 = 1 << 9;
    pub const VERSION_1: u64 = 1 << 32;
    pub const OFFERED: u64 = SEG_MAX | FLUSH | VERSION_1;
}

/// The guest's physical memory, as the device sees it.
pub trait GuestMem {
    /// Fill `dst` from guest-physical `gpa`. False if any of it is not RAM.
    fn read(&self, gpa: u64, dst: &mut [u8]) -> bool;
    /// Copy `src` to guest-physical `gpa`. False if any of it is not RAM.
    fn write(&self, gpa: u64, src: &[u8]) -> bool;
}

/// The store behind the device. Offsets are in bytes from the start of the disk.
pub trait Disk {
    /// Size in 512-byte sectors.
    fn sectors(&self) -> u64;
    /// Read `dst.len()` bytes at `offset`.
    fn read(&mut self, offset: u64, dst: &mut [u8]) -> bool;
    /// Write `src` at `offset`.
    fn write(&mut self, offset: u64, src: &[u8]) -> bool;
    /// Make completed writes durable.
    fn flush(&mut self) -> bool;
}

/// What a register write asks the caller to do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Write {
    /// Nothing more.
    Done,
    /// The driver rang the doorbell for this queue: call [`MmioBlk::process`].
    Notify(u16),
    /// Writing zero to Status reset the device.
    Reset,
}

/// What [`MmioBlk::process`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Processed {
    /// Requests completed (entries added to the used ring).
    pub completed: u32,
    /// The interrupt line should be raised.
    pub interrupt: bool,
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

/// The device: register file, one queue, interrupt status.
#[derive(Clone, Copy, Debug)]
pub struct MmioBlk {
    sectors: u64,
    status: u32,
    dev_features_sel: u32,
    drv_features_sel: u32,
    drv_features: u64,
    queue_sel: u32,
    queue: Queue,
    interrupt_status: u32,
}

const REQ_IN: u32 = 0;
const REQ_OUT: u32 = 1;
const REQ_FLUSH: u32 = 4;
const REQ_GET_ID: u32 = 8;
const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
const AVAIL_NO_INTERRUPT: u16 = 1;
/// The identity `GET_ID` returns.
const DEVICE_ID_STRING: &[u8; 20] = b"akuma-kvm-virtio-blk";

pub(crate) fn rd16(m: &dyn GuestMem, gpa: u64) -> Option<u16> {
    let mut b = [0u8; 2];
    m.read(gpa, &mut b).then(|| u16::from_le_bytes(b))
}

pub(crate) fn rd32(m: &dyn GuestMem, gpa: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    m.read(gpa, &mut b).then(|| u32::from_le_bytes(b))
}

pub(crate) fn rd64(m: &dyn GuestMem, gpa: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    m.read(gpa, &mut b).then(|| u64::from_le_bytes(b))
}

impl MmioBlk {
    /// A device for a disk of `sectors` 512-byte sectors, in its reset state.
    #[must_use]
    pub const fn new(sectors: u64) -> Self {
        Self {
            sectors,
            status: 0,
            dev_features_sel: 0,
            drv_features_sel: 0,
            drv_features: 0,
            queue_sel: 0,
            queue: Queue { num: 0, ready: false, desc: 0, avail: 0, used: 0, last_avail: 0 },
            interrupt_status: 0,
        }
    }

    /// Device reset: everything the driver configured is forgotten.
    fn reset(&mut self) {
        *self = Self::new(self.sectors);
    }

    /// The configuration space as bytes (spec 5.2.4 `virtio_blk_config`).
    fn config_byte(&self, off: u64) -> u8 {
        match off {
            0..=7 => self.sectors.to_le_bytes()[off as usize],
            12..=15 => SEG_MAX.to_le_bytes()[(off - 12) as usize],
            _ => 0,
        }
    }

    /// The interrupt line's level: any unacknowledged status bit.
    #[must_use]
    pub const fn irq_level(&self) -> bool {
        self.interrupt_status != 0
    }

    /// A read of `size` bytes (1, 2, 4 or 8) at `off` into the register block.
    #[must_use]
    pub fn read(&self, off: u64, size: u8) -> u64 {
        if off >= reg::CONFIG {
            let mut v = 0u64;
            for k in 0..u64::from(size.min(8)) {
                v |= u64::from(self.config_byte(off - reg::CONFIG + k)) << (8 * k);
            }
            return v;
        }
        // The register block is 32-bit registers; anything else reads zero.
        if size != 4 || off & 3 != 0 {
            return 0;
        }
        let q0 = self.queue_sel == 0;
        u64::from(match off {
            reg::MAGIC => MAGIC,
            reg::VERSION => 2,
            reg::DEVICE_ID => DEVICE_ID_BLOCK,
            reg::VENDOR_ID => VENDOR_ID,
            reg::DEVICE_FEATURES => match self.dev_features_sel {
                0 => feat::OFFERED as u32,
                1 => (feat::OFFERED >> 32) as u32,
                _ => 0,
            },
            reg::QUEUE_NUM_MAX => if q0 { u32::from(QUEUE_MAX) } else { 0 },
            reg::QUEUE_READY => u32::from(q0 && self.queue.ready),
            reg::INTERRUPT_STATUS => self.interrupt_status,
            reg::STATUS => self.status,
            reg::CONFIG_GENERATION => 0,
            _ => 0,
        })
    }

    /// A write of `size` bytes of `val` at `off`.
    pub fn write(&mut self, off: u64, size: u8, val: u64) -> Write {
        // Configuration space is read-only for a block device.
        if off >= reg::CONFIG || size != 4 || off & 3 != 0 {
            return Write::Done;
        }
        let v = val as u32;
        let q0 = self.queue_sel == 0;
        match off {
            reg::DEVICE_FEATURES_SEL => self.dev_features_sel = v,
            reg::DRIVER_FEATURES_SEL => self.drv_features_sel = v,
            reg::DRIVER_FEATURES => match self.drv_features_sel {
                0 => self.drv_features = (self.drv_features & !0xFFFF_FFFF) | u64::from(v),
                1 => self.drv_features = (self.drv_features & 0xFFFF_FFFF) | (u64::from(v) << 32),
                _ => {}
            },
            reg::QUEUE_SEL => self.queue_sel = v,
            reg::QUEUE_NUM if q0 => {
                // Only a power of two up to QUEUE_MAX is a usable ring.
                let n = v as u16;
                self.queue.num = if v <= u32::from(QUEUE_MAX) && n.is_power_of_two() { n } else { 0 };
            }
            reg::QUEUE_READY if q0 => {
                self.queue.ready = v & 1 != 0 && self.queue.num != 0;
                if self.queue.ready {
                    self.queue.last_avail = 0;
                }
            }
            reg::QUEUE_DESC_LOW if q0 => self.queue.desc = (self.queue.desc & !0xFFFF_FFFF) | u64::from(v),
            reg::QUEUE_DESC_HIGH if q0 => self.queue.desc = (self.queue.desc & 0xFFFF_FFFF) | (u64::from(v) << 32),
            reg::QUEUE_DRIVER_LOW if q0 => self.queue.avail = (self.queue.avail & !0xFFFF_FFFF) | u64::from(v),
            reg::QUEUE_DRIVER_HIGH if q0 => {
                self.queue.avail = (self.queue.avail & 0xFFFF_FFFF) | (u64::from(v) << 32);
            }
            reg::QUEUE_DEVICE_LOW if q0 => self.queue.used = (self.queue.used & !0xFFFF_FFFF) | u64::from(v),
            reg::QUEUE_DEVICE_HIGH if q0 => {
                self.queue.used = (self.queue.used & 0xFFFF_FFFF) | (u64::from(v) << 32);
            }
            reg::QUEUE_NOTIFY => return Write::Notify(v as u16),
            reg::INTERRUPT_ACK => self.interrupt_status &= !v,
            reg::STATUS => {
                if v == 0 {
                    self.reset();
                    return Write::Reset;
                }
                self.status = v;
                // FEATURES_OK is refused unless the driver took VERSION_1 and
                // nothing that was not offered.
                if v & status::FEATURES_OK != 0
                    && (self.drv_features & feat::VERSION_1 == 0 || self.drv_features & !feat::OFFERED != 0)
                {
                    self.status &= !status::FEATURES_OK;
                }
            }
            _ => {}
        }
        Write::Done
    }

    /// Serve every request the driver has made available. `bounce` is the
    /// transfer window between guest memory and the disk (its length bounds
    /// one disk call; 4096 is plenty).
    pub fn process(&mut self, mem: &dyn GuestMem, disk: &mut dyn Disk, bounce: &mut [u8]) -> Processed {
        let mut out = Processed::default();
        let q = self.queue;
        if !q.ready || self.status & status::DRIVER_OK == 0 || self.status & status::NEEDS_RESET != 0 {
            return out;
        }
        let Some(avail_flags) = rd16(mem, q.avail) else { return self.broken(out) };
        let Some(avail_idx) = rd16(mem, q.avail + 2) else { return self.broken(out) };
        let Some(mut used_idx) = rd16(mem, q.used + 2) else { return self.broken(out) };
        let mut last = q.last_avail;
        // A driver that claims more outstanding requests than the ring holds
        // is broken, not busy.
        if avail_idx.wrapping_sub(last) > q.num {
            return self.broken(out);
        }
        while last != avail_idx {
            let slot = u64::from(last % q.num);
            let Some(head) = rd16(mem, q.avail + 4 + 2 * slot) else { return self.broken(out) };
            let written = if head < q.num { self.request(mem, disk, bounce, head) } else { 0 };
            let used_slot = u64::from(used_idx % q.num);
            let mut elem = [0u8; 8];
            elem[..4].copy_from_slice(&u32::from(head).to_le_bytes());
            elem[4..].copy_from_slice(&written.to_le_bytes());
            if !mem.write(q.used + 4 + 8 * used_slot, &elem) {
                return self.broken(out);
            }
            used_idx = used_idx.wrapping_add(1);
            last = last.wrapping_add(1);
            out.completed += 1;
            // The index is published after the element: a driver that sees the
            // new index sees the element.
            if !mem.write(q.used + 2, &used_idx.to_le_bytes()) {
                return self.broken(out);
            }
            self.queue.last_avail = last;
        }
        if out.completed > 0 && avail_flags & AVAIL_NO_INTERRUPT == 0 {
            self.interrupt_status |= 1;
            out.interrupt = true;
        }
        out
    }

    fn broken(&mut self, out: Processed) -> Processed {
        self.status |= status::NEEDS_RESET;
        // Configuration-change interrupt (bit 1): tell the driver to reset us.
        self.interrupt_status |= 2;
        Processed { completed: out.completed, interrupt: true }
    }

    /// Serve the request whose chain starts at `head`. Returns the number of
    /// bytes written into guest memory (the used element's `len`).
    fn request(&self, mem: &dyn GuestMem, disk: &mut dyn Disk, bounce: &mut [u8], head: u16) -> u32 {
        let q = self.queue;
        let desc = |i: u16| -> Option<(u64, u32, u16, u16)> {
            let base = q.desc + 16 * u64::from(i);
            Some((rd64(mem, base)?, rd32(mem, base + 8)?, rd16(mem, base + 12)?, rd16(mem, base + 14)?))
        };
        // Header: the first descriptor, device-readable, 16 bytes.
        let Some((haddr, hlen, hflags, hnext)) = desc(head) else { return 0 };
        let mut hdr = [0u8; 16];
        if hlen < 16 || hflags & DESC_WRITE != 0 || !mem.read(haddr, &mut hdr) {
            return 0;
        }
        let rtype = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        let sector = u64::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11], hdr[12], hdr[13], hdr[14], hdr[15]]);

        // Walk the rest of the chain once, remembering the last descriptor (the
        // status byte lives in its final byte) and doing the data transfers of
        // the middle ones as we go.
        let mut status_addr = None;
        let mut result = S_OK;
        let mut written = 0u32;
        let mut disk_off = sector.checked_mul(SECTOR);
        let (mut cur_flags, mut cur) = (hflags, hnext);
        let mut hops = 1u32;
        while cur_flags & DESC_NEXT != 0 {
            hops += 1;
            if hops > u32::from(q.num) || cur >= q.num {
                return written;
            }
            let Some((addr, len, flags, next)) = desc(cur) else { return written };
            let last = flags & DESC_NEXT == 0;
            if last {
                // The status descriptor: device-writable, at least one byte.
                if flags & DESC_WRITE == 0 || len == 0 {
                    return written;
                }
                status_addr = Some(addr + u64::from(len) - 1);
            } else if result == S_OK && matches!(rtype, REQ_IN | REQ_OUT | REQ_GET_ID) {
                let to_guest = rtype != REQ_OUT;
                if to_guest != (flags & DESC_WRITE != 0) {
                    // Direction contradicts the request type.
                    result = S_IOERR;
                } else if rtype == REQ_GET_ID {
                    let n = (len as usize).min(DEVICE_ID_STRING.len());
                    if mem.write(addr, &DEVICE_ID_STRING[..n]) {
                        written += n as u32;
                    } else {
                        result = S_IOERR;
                    }
                } else {
                    let ok = self.transfer(mem, disk, bounce, rtype == REQ_IN, addr, len, &mut disk_off);
                    if ok {
                        if to_guest {
                            written += len;
                        }
                    } else {
                        result = S_IOERR;
                    }
                }
            }
            cur_flags = flags;
            cur = next;
        }
        match rtype {
            REQ_IN | REQ_OUT | REQ_GET_ID => {}
            REQ_FLUSH => {
                if !disk.flush() {
                    result = S_IOERR;
                }
            }
            _ => result = S_UNSUPP,
        }
        if let Some(a) = status_addr
            && mem.write(a, &[result])
        {
            written += 1;
        }
        written
    }

    /// Move `len` bytes between guest memory at `addr` and the disk at
    /// `*disk_off`, through `bounce`; advances `*disk_off`.
    #[allow(clippy::too_many_arguments)]
    fn transfer(
        &self,
        mem: &dyn GuestMem,
        disk: &mut dyn Disk,
        bounce: &mut [u8],
        read: bool,
        addr: u64,
        len: u32,
        disk_off: &mut Option<u64>,
    ) -> bool {
        let Some(mut off) = *disk_off else { return false };
        let total = u64::from(len);
        // Requests are whole sectors, inside the disk.
        if total % SECTOR != 0 || bounce.is_empty() {
            return false;
        }
        let end = off.checked_add(total);
        if end.is_none_or(|e| e > self.sectors * SECTOR) {
            return false;
        }
        let mut done = 0u64;
        while done < total {
            let n = (total - done).min(bounce.len() as u64) as usize;
            let chunk = &mut bounce[..n];
            if read {
                if !disk.read(off, chunk) || !mem.write(addr + done, chunk) {
                    return false;
                }
            } else if !mem.read(addr + done, chunk) || !disk.write(off, chunk) {
                return false;
            }
            done += n as u64;
            off += n as u64;
        }
        *disk_off = Some(off);
        true
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use core::cell::RefCell;
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
        fn put16(&self, gpa: u64, v: u16) {
            self.put(gpa, &v.to_le_bytes());
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

    struct Ram {
        bytes: Vec<u8>,
        flushes: u32,
        fail: bool,
    }
    impl Disk for Ram {
        fn sectors(&self) -> u64 {
            self.bytes.len() as u64 / SECTOR
        }
        fn read(&mut self, o: u64, d: &mut [u8]) -> bool {
            !self.fail && self.bytes.get(o as usize..o as usize + d.len()).is_some_and(|s| {
                d.copy_from_slice(s);
                true
            })
        }
        fn write(&mut self, o: u64, s: &[u8]) -> bool {
            !self.fail && self.bytes.get_mut(o as usize..o as usize + s.len()).is_some_and(|d| {
                d.copy_from_slice(s);
                true
            })
        }
        fn flush(&mut self) -> bool {
            self.flushes += 1;
            !self.fail
        }
    }

    const DESC: u64 = 0x1000;
    const AVAIL: u64 = 0x2000;
    const USED: u64 = 0x3000;
    const NUM: u16 = 8;

    /// A driver's view: negotiate, set up queue 0 with `NUM` entries.
    fn bring_up(sectors: u64) -> MmioBlk {
        let mut d = MmioBlk::new(sectors);
        assert_eq!(d.read(reg::MAGIC, 4), u64::from(MAGIC));
        assert_eq!(d.read(reg::VERSION, 4), 2);
        assert_eq!(d.read(reg::DEVICE_ID, 4), 2);
        d.write(reg::STATUS, 4, u64::from(status::ACKNOWLEDGE));
        d.write(reg::STATUS, 4, u64::from(status::ACKNOWLEDGE | status::DRIVER));
        d.write(reg::DRIVER_FEATURES_SEL, 4, 0);
        d.write(reg::DRIVER_FEATURES, 4, 0);
        d.write(reg::DRIVER_FEATURES_SEL, 4, 1);
        d.write(reg::DRIVER_FEATURES, 4, 1); // VERSION_1
        d.write(reg::STATUS, 4, u64::from(status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK));
        assert_ne!(d.read(reg::STATUS, 4) & u64::from(status::FEATURES_OK), 0);
        d.write(reg::QUEUE_SEL, 4, 0);
        assert_eq!(d.read(reg::QUEUE_NUM_MAX, 4), u64::from(QUEUE_MAX));
        d.write(reg::QUEUE_NUM, 4, u64::from(NUM));
        for (lo, hi, a) in [
            (reg::QUEUE_DESC_LOW, reg::QUEUE_DESC_HIGH, DESC),
            (reg::QUEUE_DRIVER_LOW, reg::QUEUE_DRIVER_HIGH, AVAIL),
            (reg::QUEUE_DEVICE_LOW, reg::QUEUE_DEVICE_HIGH, USED),
        ] {
            d.write(lo, 4, a & 0xFFFF_FFFF);
            d.write(hi, 4, a >> 32);
        }
        d.write(reg::QUEUE_READY, 4, 1);
        d.write(reg::STATUS, 4, 0xF);
        d
    }

    fn put_desc(m: &Mem, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let b = DESC + 16 * u64::from(i);
        m.put(b, &addr.to_le_bytes());
        m.put(b + 8, &len.to_le_bytes());
        m.put16(b + 12, flags);
        m.put16(b + 14, next);
    }

    /// Place a request (header, `data` descriptors, status) as chain `head`.
    fn submit(m: &Mem, head: u16, rtype: u32, sector: u64, data: &[(u64, u32, bool)], avail_slot: u16, avail_idx: u16) {
        let hdr = 0x4000 + 0x40 * u64::from(head);
        let mut h = [0u8; 16];
        h[..4].copy_from_slice(&rtype.to_le_bytes());
        h[8..].copy_from_slice(&sector.to_le_bytes());
        m.put(hdr, &h);
        let status = hdr + 0x20;
        put_desc(m, head, hdr, 16, DESC_NEXT, head + 1);
        let mut i = head + 1;
        for (addr, len, write) in data {
            put_desc(m, i, *addr, *len, DESC_NEXT | if *write { DESC_WRITE } else { 0 }, i + 1);
            i += 1;
        }
        put_desc(m, i, status, 1, DESC_WRITE, 0);
        m.put16(AVAIL + 4 + 2 * u64::from(avail_slot), head);
        m.put16(AVAIL + 2, avail_idx);
    }

    fn status_of(m: &Mem, head: u16) -> u8 {
        m.get(0x4000 + 0x40 * u64::from(head) + 0x20, 1)[0]
    }

    fn disk(sectors: u64) -> Ram {
        let mut bytes = vec![0u8; (sectors * SECTOR) as usize];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        Ram { bytes, flushes: 0, fail: false }
    }

    #[test]
    fn config_space_reports_capacity_and_seg_max() {
        let d = MmioBlk::new(0x1_2345_6789);
        assert_eq!(d.read(reg::CONFIG, 8), 0x1_2345_6789);
        assert_eq!(d.read(reg::CONFIG, 4), 0x2345_6789);
        assert_eq!(d.read(reg::CONFIG + 4, 4), 1);
        assert_eq!(d.read(reg::CONFIG + 12, 4), u64::from(SEG_MAX));
        assert_eq!(d.read(reg::CONFIG + 1, 1), 0x67);
    }

    #[test]
    fn features_are_offered_in_two_windows() {
        let mut d = MmioBlk::new(8);
        d.write(reg::DEVICE_FEATURES_SEL, 4, 0);
        assert_eq!(d.read(reg::DEVICE_FEATURES, 4), (1 << 2) | (1 << 9));
        d.write(reg::DEVICE_FEATURES_SEL, 4, 1);
        assert_eq!(d.read(reg::DEVICE_FEATURES, 4), 1);
        d.write(reg::DEVICE_FEATURES_SEL, 4, 2);
        assert_eq!(d.read(reg::DEVICE_FEATURES, 4), 0);
    }

    #[test]
    fn features_ok_is_refused_without_version_1_or_with_unoffered_bits() {
        let mut d = MmioBlk::new(8);
        d.write(reg::STATUS, 4, 3);
        d.write(reg::STATUS, 4, 0xB);
        assert_eq!(d.read(reg::STATUS, 4) & 8, 0, "no VERSION_1");
        d.write(reg::DRIVER_FEATURES_SEL, 4, 1);
        d.write(reg::DRIVER_FEATURES, 4, 1);
        d.write(reg::DRIVER_FEATURES_SEL, 4, 0);
        d.write(reg::DRIVER_FEATURES, 4, 1 << 5); // not offered
        d.write(reg::STATUS, 4, 0xB);
        assert_eq!(d.read(reg::STATUS, 4) & 8, 0, "unoffered feature");
    }

    #[test]
    fn only_queue_0_exists_and_size_must_be_a_power_of_two() {
        let mut d = MmioBlk::new(8);
        d.write(reg::QUEUE_SEL, 4, 1);
        assert_eq!(d.read(reg::QUEUE_NUM_MAX, 4), 0);
        d.write(reg::QUEUE_SEL, 4, 0);
        d.write(reg::QUEUE_NUM, 4, 6);
        d.write(reg::QUEUE_READY, 4, 1);
        assert_eq!(d.read(reg::QUEUE_READY, 4), 0, "6 is not a ring size");
        d.write(reg::QUEUE_NUM, 4, u64::from(QUEUE_MAX) * 2);
        d.write(reg::QUEUE_READY, 4, 1);
        assert_eq!(d.read(reg::QUEUE_READY, 4), 0, "too big");
    }

    #[test]
    fn a_read_request_fills_the_buffer_and_completes() {
        let (m, mut dk, mut b) = (Mem::new(0x10000), disk(64), [0u8; 4096]);
        let mut d = bring_up(dk.sectors());
        submit(&m, 0, REQ_IN, 3, &[(0x8000, 1024, true)], 0, 1);
        assert_eq!(d.write(reg::QUEUE_NOTIFY, 4, 0), Write::Notify(0));
        let r = d.process(&m, &mut dk, &mut b);
        assert_eq!(r, Processed { completed: 1, interrupt: true });
        assert_eq!(m.get(0x8000, 1024), dk.bytes[3 * 512..3 * 512 + 1024]);
        assert_eq!(status_of(&m, 0), S_OK);
        // Used ring: idx 1, element {id 0, len 1024 + status byte}.
        assert_eq!(m.get(USED + 2, 2), 1u16.to_le_bytes());
        assert_eq!(m.get(USED + 4, 8), [0, 0, 0, 0, 0x01, 0x04, 0, 0]);
        assert!(d.irq_level());
        d.write(reg::INTERRUPT_ACK, 4, 1);
        assert!(!d.irq_level());
    }

    #[test]
    fn a_write_request_reaches_the_disk() {
        let (m, mut dk, mut b) = (Mem::new(0x10000), disk(64), [0u8; 4096]);
        let mut d = bring_up(dk.sectors());
        m.put(0x8000, &[0xAB; 512]);
        m.put(0x8200, &[0xCD; 512]);
        submit(&m, 0, REQ_OUT, 10, &[(0x8000, 512, false), (0x8200, 512, false)], 0, 1);
        d.process(&m, &mut dk, &mut b);
        assert_eq!(status_of(&m, 0), S_OK);
        assert_eq!(&dk.bytes[10 * 512..11 * 512], &[0xAB; 512]);
        assert_eq!(&dk.bytes[11 * 512..12 * 512], &[0xCD; 512]);
        // Only the status byte was written into guest memory.
        assert_eq!(m.get(USED + 8, 4), 1u32.to_le_bytes());
    }

    #[test]
    fn transfers_larger_than_the_bounce_window_are_chunked() {
        let (m, mut dk, mut b) = (Mem::new(0x20000), disk(64), [0u8; 512]);
        let mut d = bring_up(dk.sectors());
        submit(&m, 0, REQ_IN, 0, &[(0x8000, 4096, true)], 0, 1);
        d.process(&m, &mut dk, &mut b);
        assert_eq!(status_of(&m, 0), S_OK);
        assert_eq!(m.get(0x8000, 4096), dk.bytes[..4096]);
    }

    #[test]
    fn flush_calls_the_disk_and_unknown_types_are_unsupported() {
        let (m, mut dk, mut b) = (Mem::new(0x10000), disk(8), [0u8; 4096]);
        let mut d = bring_up(dk.sectors());
        submit(&m, 0, REQ_FLUSH, 0, &[], 0, 1);
        d.process(&m, &mut dk, &mut b);
        assert_eq!((status_of(&m, 0), dk.flushes), (S_OK, 1));
        submit(&m, 2, 99, 0, &[], 1, 2);
        d.process(&m, &mut dk, &mut b);
        assert_eq!(status_of(&m, 2), S_UNSUPP);
    }

    #[test]
    fn get_id_returns_the_identity_string() {
        let (m, mut dk, mut b) = (Mem::new(0x10000), disk(8), [0u8; 4096]);
        let mut d = bring_up(dk.sectors());
        submit(&m, 0, REQ_GET_ID, 0, &[(0x8000, 20, true)], 0, 1);
        d.process(&m, &mut dk, &mut b);
        assert_eq!(m.get(0x8000, 20), DEVICE_ID_STRING);
        assert_eq!(status_of(&m, 0), S_OK);
    }

    #[test]
    fn out_of_range_misaligned_and_failing_requests_report_ioerr() {
        let (m, mut dk, mut b) = (Mem::new(0x10000), disk(8), [0u8; 4096]);
        let mut d = bring_up(dk.sectors());
        // Past the end of the disk.
        submit(&m, 0, REQ_IN, 7, &[(0x8000, 1024, true)], 0, 1);
        d.process(&m, &mut dk, &mut b);
        assert_eq!(status_of(&m, 0), S_IOERR);
        // Not a whole number of sectors.
        submit(&m, 2, REQ_IN, 0, &[(0x8000, 100, true)], 1, 2);
        d.process(&m, &mut dk, &mut b);
        assert_eq!(status_of(&m, 2), S_IOERR);
        // Sector number that overflows the byte offset.
        submit(&m, 4, REQ_IN, u64::MAX, &[(0x8000, 512, true)], 2, 3);
        d.process(&m, &mut dk, &mut b);
        assert_eq!(status_of(&m, 4), S_IOERR);
        // Wrong direction: a read into a device-readable buffer.
        submit(&m, 5, REQ_IN, 0, &[(0x8000, 512, false)], 3, 4);
        d.process(&m, &mut dk, &mut b);
        assert_eq!(status_of(&m, 5), S_IOERR);
        // The disk itself fails.
        dk.fail = true;
        submit(&m, 0, REQ_OUT, 0, &[(0x8000, 512, false)], 4, 5);
        d.process(&m, &mut dk, &mut b);
        assert_eq!(status_of(&m, 0), S_IOERR);
        assert_eq!(m.get(USED + 2, 2), 5u16.to_le_bytes(), "every request completed");
    }

    #[test]
    fn several_requests_in_one_notify_and_ring_wraparound() {
        let (m, mut dk, mut b) = (Mem::new(0x10000), disk(64), [0u8; 4096]);
        let mut d = bring_up(dk.sectors());
        // Fill and drain the 8-entry ring more than once.
        for round in 0..3u16 {
            for k in 0..4u16 {
                let head = k * 2;
                let idx = round * 4 + k;
                // Each chain uses descriptors head, head+1 (and one more for data).
                submit(&m, head, REQ_IN, u64::from(idx), &[], idx % NUM, idx + 1);
            }
            let r = d.process(&m, &mut dk, &mut b);
            assert_eq!(r.completed, 4);
            for k in 0..4u16 {
                assert_eq!(status_of(&m, k * 2), S_OK);
            }
            assert_eq!(m.get(USED + 2, 2), ((round + 1) * 4).to_le_bytes());
        }
        // Nothing new: nothing to do, no interrupt.
        assert_eq!(d.process(&m, &mut dk, &mut b), Processed::default());
    }

    #[test]
    fn no_interrupt_flag_suppresses_the_line() {
        let (m, mut dk, mut b) = (Mem::new(0x10000), disk(8), [0u8; 4096]);
        let mut d = bring_up(dk.sectors());
        m.put16(AVAIL, AVAIL_NO_INTERRUPT);
        submit(&m, 0, REQ_FLUSH, 0, &[], 0, 1);
        let r = d.process(&m, &mut dk, &mut b);
        assert_eq!(r, Processed { completed: 1, interrupt: false });
        assert!(!d.irq_level());
    }

    #[test]
    fn a_broken_ring_needs_a_reset() {
        let (m, mut dk, mut b) = (Mem::new(0x10000), disk(8), [0u8; 4096]);
        let mut d = bring_up(dk.sectors());
        // The driver claims 9 outstanding requests on an 8-entry ring.
        m.put16(AVAIL + 2, 9);
        let r = d.process(&m, &mut dk, &mut b);
        assert!(r.interrupt);
        assert_ne!(d.read(reg::STATUS, 4) & u64::from(status::NEEDS_RESET), 0);
        assert_eq!(d.read(reg::INTERRUPT_STATUS, 4) & 2, 2, "configuration-change interrupt");
        // A ring outside RAM is as broken.
        let mut d = bring_up(dk.sectors());
        d.write(reg::QUEUE_DRIVER_LOW, 4, 0xFFFF_0000);
        m.put16(AVAIL + 2, 0);
        assert!(d.process(&m, &mut dk, &mut b).interrupt);
        // Writing 0 to Status resets it, and forgets the queue.
        assert_eq!(d.write(reg::STATUS, 4, 0), Write::Reset);
        assert_eq!(d.read(reg::QUEUE_READY, 4), 0);
        assert_eq!(d.read(reg::STATUS, 4), 0);
    }

    #[test]
    fn a_looping_chain_terminates() {
        let (m, mut dk, mut b) = (Mem::new(0x10000), disk(8), [0u8; 4096]);
        let mut d = bring_up(dk.sectors());
        submit(&m, 0, REQ_IN, 0, &[], 0, 1);
        // Descriptor 1 (the "status") now chains back to 0, forever.
        put_desc(&m, 1, 0x4020, 1, DESC_NEXT | DESC_WRITE, 0);
        let r = d.process(&m, &mut dk, &mut b);
        assert_eq!(r.completed, 1);
    }

    #[test]
    fn nothing_is_served_before_driver_ok_or_queue_ready() {
        let (m, mut dk, mut b) = (Mem::new(0x10000), disk(8), [0u8; 4096]);
        let mut d = MmioBlk::new(dk.sectors());
        submit(&m, 0, REQ_FLUSH, 0, &[], 0, 1);
        assert_eq!(d.process(&m, &mut dk, &mut b), Processed::default());
        assert_eq!(dk.flushes, 0);
    }
}

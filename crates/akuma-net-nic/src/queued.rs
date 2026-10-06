//! A NIC that is two frame queues: [`ExternalDevice::Queued`](crate::ExternalDevice).
//!
//! For a link whose driver runs on its own schedule rather than inside
//! `iface.poll()` — the amd64 wifi station (`amd64/src/rtw89_sta.rs`) is the
//! first: it owns the radio from its own daemon, joins, handshakes, and only
//! then has Ethernet frames to give. Coupling that driver to this crate would
//! drag the card into the network stack; instead the two meet at a
//! [`FrameQueues`]:
//!
//! - the driver [`deliver`](FrameQueues::deliver)s each received frame (as
//!   Ethernet: destination, source, ethertype, payload) and drains
//!   [`next_transmit`](FrameQueues::next_transmit) for what the stack sent;
//! - the stack sees an ordinary device — [`QueuedDevice`] takes from the
//!   receive queue and puts into the transmit queue.
//!
//! # No spinning
//!
//! Both sides only ever `try_lock`. The queues sit between two threads that
//! may share one core (`nosmp` on the metal), and a thread spinning on a lock
//! its preempted holder cannot release until the spinner yields is a hang. A
//! contended push drops the frame (counted); a contended pop answers "nothing
//! yet". TCP and DHCP both retransmit, so a lost frame is latency, not loss.
//!
//! # Link changes
//!
//! A link that joins and drops (wifi) tells the stack through an atomic
//! **generation**: the driver calls [`FrameQueues::link_changed`] when the
//! link comes up or goes away, and the stack's poll loop, which reads
//! [`ExternalDevice::link_generation`](crate::ExternalDevice::link_generation)
//! every lap, restarts DHCP when it moves — so a join gets an address at once
//! instead of after the client's backoff, and a rejoin (perhaps to another
//! network) never keeps a stale lease. No callback: the driver never runs the
//! stack's code, so no lock order between them exists to get wrong.
//!
//! No allocation: fixed slots of [`FRAME_MAX`] bytes, [`SLOTS`] per direction.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use smoltcp::phy::DeviceCapabilities;
use spinning_top::Spinlock;

/// Bytes per frame slot: an Ethernet frame of MTU 1500 (1514), rounded up.
pub const FRAME_MAX: usize = 1536;
/// Frames queued per direction.
pub const SLOTS: usize = 16;

/// A FIFO of up to [`SLOTS`] frames.
struct Ring {
    buf: [[u8; FRAME_MAX]; SLOTS],
    len: [u16; SLOTS],
    head: usize,
    count: usize,
}

impl Ring {
    // Only ever evaluated to initialise a `static` (`FrameQueues::new` is
    // `const`), never built on a stack at run time.
    #[allow(clippy::large_stack_arrays)]
    const fn new() -> Self {
        Self { buf: [[0; FRAME_MAX]; SLOTS], len: [0; SLOTS], head: 0, count: 0 }
    }

    fn push(&mut self, f: &[u8]) -> bool {
        if self.count == SLOTS || f.len() > FRAME_MAX {
            return false;
        }
        let at = (self.head + self.count) % SLOTS;
        self.buf[at][..f.len()].copy_from_slice(f);
        self.len[at] = f.len() as u16;
        self.count += 1;
        true
    }

    fn pop(&mut self, out: &mut [u8]) -> Option<usize> {
        if self.count == 0 {
            return None;
        }
        let n = usize::from(self.len[self.head]).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.head][..n]);
        self.head = (self.head + 1) % SLOTS;
        self.count -= 1;
        Some(n)
    }
}

/// The meeting point of a driver and the stack. One `static` per link.
pub struct FrameQueues {
    rx: Spinlock<Ring>,
    tx: Spinlock<Ring>,
    mac: Spinlock<[u8; 6]>,
    /// Bumped on every link up/down ([`FrameQueues::link_changed`]).
    link_gen: AtomicU32,
    /// Rung after the stack queues a frame ([`FrameQueues::on_transmit`]):
    /// the driver runs on its own schedule, and a frame left for its next
    /// timed lap waits as long as that lap is away.
    ///
    /// **Never rung where the frame is queued.** That is inside the stack's
    /// `NETWORK` critical section with preemption off, and waking another
    /// thread from there wedged ryzen whole (2026-10-06: console, sshd and the
    /// link all stopped; the watchdog saw ticks, so nothing reset it). The
    /// queue only sets [`FrameQueues::tx_pending`]; the stack calls
    /// [`ring_deferred`] once the lock is released.
    tx_doorbell: Spinlock<Option<fn()>>,
    tx_pending: AtomicBool,
    /// Frames dropped because a queue was full or busy.
    pub rx_dropped: AtomicU32,
    pub tx_dropped: AtomicU32,
}

impl Default for FrameQueues {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameQueues {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            rx: Spinlock::new(Ring::new()),
            tx: Spinlock::new(Ring::new()),
            mac: Spinlock::new([0; 6]),
            link_gen: AtomicU32::new(0),
            tx_doorbell: Spinlock::new(None),
            tx_pending: AtomicBool::new(false),
            rx_dropped: AtomicU32::new(0),
            tx_dropped: AtomicU32::new(0),
        }
    }

    /// The link's MAC, before the stack is built on it.
    pub fn set_mac(&self, mac: [u8; 6]) {
        *self.mac.lock() = mac;
    }

    /// Driver side: `ring` is called after the stack has queued frames —
    /// typically a wake of the driver's thread — from [`ring_deferred`], with
    /// no lock of the stack's held. This also makes `self` the queues
    /// [`ring_deferred`] looks at.
    pub fn on_transmit(&'static self, ring: fn()) {
        *self.tx_doorbell.lock() = Some(ring);
        *ACTIVE.lock() = Some(self);
    }

    /// Ring the doorbell if frames were queued since it last rang.
    pub fn ring_if_pending(&self) {
        if self.tx_pending.swap(false, Ordering::AcqRel) {
            let ring = self.tx_doorbell.try_lock().and_then(|d| *d);
            if let Some(ring) = ring {
                ring();
            }
        }
    }

    /// Driver side: the link came up or went away. The stack restarts DHCP
    /// on its next poll.
    pub fn link_changed(&self) {
        self.link_gen.fetch_add(1, Ordering::Release);
    }

    /// The link generation: moves on every [`FrameQueues::link_changed`].
    #[must_use]
    pub fn link_generation(&self) -> u32 {
        self.link_gen.load(Ordering::Acquire)
    }

    /// Driver side: hand a received Ethernet frame to the stack. `false` if
    /// it was dropped (queue full or busy, or longer than [`FRAME_MAX`]).
    pub fn deliver(&self, frame: &[u8]) -> bool {
        let ok = self.rx.try_lock().is_some_and(|mut r| r.push(frame));
        if !ok {
            self.rx_dropped.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }

    /// Driver side: the next Ethernet frame the stack sent, into `out`.
    pub fn next_transmit(&self, out: &mut [u8]) -> Option<usize> {
        self.tx.try_lock()?.pop(out)
    }

    /// Driver side: forget every queued transmit (the link went down).
    pub fn flush_transmit(&self) {
        if let Some(mut t) = self.tx.try_lock() {
            t.head = 0;
            t.count = 0;
        }
    }
}

/// The queues whose driver registered a doorbell ([`FrameQueues::on_transmit`]).
static ACTIVE: Spinlock<Option<&'static FrameQueues>> = Spinlock::new(None);

/// Stack side: ring the registered driver's doorbell if the stack queued
/// frames for it. Call with no stack lock held — `smoltcp_net::poll` does,
/// right after releasing `NETWORK`.
pub fn ring_deferred() {
    let q = ACTIVE.try_lock().and_then(|a| *a);
    if let Some(q) = q {
        q.ring_if_pending();
    }
}

/// The stack's side of a [`FrameQueues`].
pub struct QueuedDevice {
    q: &'static FrameQueues,
    rx_scratch: [u8; FRAME_MAX],
    tx_scratch: [u8; FRAME_MAX],
}

impl QueuedDevice {
    #[must_use]
    pub const fn new(q: &'static FrameQueues) -> Self {
        Self { q, rx_scratch: [0; FRAME_MAX], tx_scratch: [0; FRAME_MAX] }
    }

    #[must_use]
    pub fn mac_address(&self) -> [u8; 6] {
        *self.q.mac.lock()
    }

    #[must_use]
    pub fn link_generation(&self) -> u32 {
        self.q.link_generation()
    }

    #[allow(clippy::unused_self)] // the same shape as every other device's
    pub(crate) fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1514;
        caps
    }

    /// The next received frame, in this device's scratch: valid until the
    /// next call.
    pub(crate) fn take_rx_frame(&mut self) -> Option<(*mut u8, usize)> {
        let n = self.q.rx.try_lock()?.pop(&mut self.rx_scratch)?;
        Some((self.rx_scratch.as_mut_ptr(), n))
    }

    pub(crate) fn emit_frame<R>(
        &mut self,
        len: usize,
        fill: impl FnOnce(&mut [u8]) -> R,
        divert: impl FnOnce(&[u8]) -> bool,
    ) -> R {
        let end = len.min(FRAME_MAX);
        let res = fill(&mut self.tx_scratch[..end]);
        if divert(&self.tx_scratch[..end]) {
            return res;
        }
        let ok = self.q.tx.try_lock().is_some_and(|mut t| t.push(&self.tx_scratch[..end]));
        if ok {
            // Rung later, outside the stack's lock (`ring_deferred`).
            self.q.tx_pending.store(true, Ordering::Release);
        } else {
            self.q.tx_dropped.fetch_add(1, Ordering::Relaxed);
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static Q: FrameQueues = FrameQueues::new();

    #[test]
    fn frames_cross_in_order_and_a_full_queue_drops() {
        let q: &'static FrameQueues = Box::leak(Box::new(FrameQueues::new()));
        let mut dev = QueuedDevice::new(q);
        assert!(dev.take_rx_frame().is_none());
        for i in 0..SLOTS {
            assert!(q.deliver(&[i as u8; 60]));
        }
        assert!(!q.deliver(&[0xff; 60]), "a full queue refuses");
        assert_eq!(q.rx_dropped.load(Ordering::Relaxed), 1);
        for i in 0..SLOTS {
            let (p, n) = dev.take_rx_frame().unwrap();
            // SAFETY: the scratch the device just filled, `n` bytes long.
            let f = unsafe { core::slice::from_raw_parts(p, n) };
            assert_eq!((n, f[0]), (60, i as u8));
        }
        assert!(dev.take_rx_frame().is_none());

        dev.emit_frame(100, |b| b.fill(7), |_| false);
        dev.emit_frame(10, |b| b.fill(8), |_| true); // diverted: never queued
        let mut out = [0u8; FRAME_MAX];
        assert_eq!(q.next_transmit(&mut out), Some(100));
        assert_eq!(out[99], 7);
        assert_eq!(q.next_transmit(&mut out), None);
    }

    #[test]
    fn a_busy_queue_never_blocks_either_side() {
        Q.set_mac([2, 0, 0, 0, 0, 9]);
        let held = Q.rx.lock();
        assert!(!Q.deliver(&[1; 60]), "busy: dropped, not waited for");
        drop(held);
        let mut dev = QueuedDevice::new(&Q);
        assert_eq!(dev.mac_address(), [2, 0, 0, 0, 0, 9]);
        let held = Q.tx.lock();
        dev.emit_frame(60, |b| b.fill(1), |_| false);
        drop(held);
        assert_eq!(Q.tx_dropped.load(Ordering::Relaxed), 1);
        let mut out = [0u8; FRAME_MAX];
        assert_eq!(Q.next_transmit(&mut out), None);
    }

    #[test]
    fn the_doorbell_rings_after_the_lock_once_for_all_queued_frames() {
        use core::sync::atomic::AtomicU32;
        static RUNG: AtomicU32 = AtomicU32::new(0);
        fn ring() {
            RUNG.fetch_add(1, Ordering::Relaxed);
        }
        let q: &'static FrameQueues = Box::leak(Box::new(FrameQueues::new()));
        q.on_transmit(ring);
        let mut dev = QueuedDevice::new(q);
        dev.emit_frame(60, |b| b.fill(1), |_| false);
        dev.emit_frame(60, |b| b.fill(2), |_| false);
        dev.emit_frame(60, |b| b.fill(3), |_| true); // diverted: no frame
        // Nothing rings where frames are queued (inside the stack's lock)...
        assert_eq!(RUNG.load(Ordering::Relaxed), 0);
        // ...but once, after, for everything queued.
        q.ring_if_pending();
        q.ring_if_pending();
        assert_eq!(RUNG.load(Ordering::Relaxed), 1);
        dev.emit_frame(60, |b| b.fill(4), |_| true);
        q.ring_if_pending();
        assert_eq!(RUNG.load(Ordering::Relaxed), 1, "a diverted frame queues nothing");
    }

    #[test]
    fn link_changes_move_the_generation_the_device_reports() {
        let q: &'static FrameQueues = Box::leak(Box::new(FrameQueues::new()));
        let dev = QueuedDevice::new(q);
        let g0 = dev.link_generation();
        q.link_changed();
        q.link_changed();
        assert_eq!(dev.link_generation(), g0.wrapping_add(2));
    }

    #[test]
    fn oversized_frames_are_refused() {
        let q: &'static FrameQueues = Box::leak(Box::new(FrameQueues::new()));
        assert!(!q.deliver(&[0; FRAME_MAX + 1]));
        q.deliver(&[3; 20]);
        q.flush_transmit();
        let mut out = [0u8; FRAME_MAX];
        assert_eq!(q.next_transmit(&mut out), None);
    }

    extern crate std;
    use std::boxed::Box;
}

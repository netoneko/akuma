//! The split virtqueue, driver side (virtio 1.x §2.7).
//!
//! Three areas in memory the device also reads and writes:
//!
//! ```text
//! descriptor table  Q x 16 B   addr u64 | len u32 | flags u16 | next u16      (driver writes)
//! available ring    6 + 2Q B   flags u16 | idx u16 | ring[Q] u16 | used_event  (driver writes)
//! used ring         6 + 8Q B   flags u16 | idx u16 | ring[Q] {id u32, len u32} (device writes)
//! ```
//!
//! [`SplitQueue`] is the driver's bookkeeping: which descriptors are free, which
//! chain each in-flight head owns, and the two free-running 16-bit indices. It
//! never reads a descriptor back from shared memory: the free list and the chain
//! links are shadowed here, so a device that scribbles on the table cannot steer
//! the driver's allocator. The only device-written values it reads are the used
//! ring's `idx` and elements, and an element naming a head that is not in flight
//! is refused ([`UsedError`]) rather than freed.
//!
//! Memory is reached through [`QueueMem`] in 16-bit cells, the width of the
//! fields a device and driver race on (`avail.idx`, `used.idx`); wider fields
//! are composed little-endian. Ordering is a `SeqCst` fence before an index is
//! published and after one is observed (§2.7.13, §2.7.14).

use core::sync::atomic::{Ordering, fence};

/// Descriptor flag: the chain continues at `next`.
pub const DESC_F_NEXT: u16 = 1;
/// Descriptor flag: the device writes this buffer (otherwise it reads it).
pub const DESC_F_WRITE: u16 = 2;
/// Used-ring flag: the device asks not to be notified. Advisory without `EVENT_IDX`.
pub const USED_F_NO_NOTIFY: u16 = 1;

/// The memory a queue lives in, as 16-bit cells.
///
/// `word` is a cell index: byte offset `2 * word` from the start of the queue's
/// memory (the descriptor table). A device may write any cell at any moment, so
/// an implementation over shared memory must use volatile accesses.
pub trait QueueMem {
    fn read(&self, word: usize) -> u16;
    fn write(&self, word: usize, v: u16);
}

/// Byte offsets of the three areas for a queue of `q` entries, laid out
/// contiguously: table at 0 (16-aligned), available ring after it (2-aligned),
/// used ring rounded up to 4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub desc: usize,
    pub avail: usize,
    pub used: usize,
    /// Total bytes, from the table to the end of the used ring.
    pub bytes: usize,
}

impl Layout {
    #[must_use]
    pub const fn new(q: usize) -> Self {
        let avail = 16 * q;
        let used = (avail + 6 + 2 * q).next_multiple_of(4);
        Self { desc: 0, avail, used, bytes: used + 6 + 8 * q }
    }

    /// 16-bit cells needed to back the queue.
    #[must_use]
    pub const fn words(&self) -> usize {
        self.bytes.div_ceil(2)
    }
}

/// One buffer of a request, as the device will see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buf {
    /// Device-visible (physical) address.
    pub addr: u64,
    pub len: u32,
    /// The device writes this buffer; otherwise it only reads it.
    pub device_writes: bool,
}

/// Why [`SplitQueue::add`] refused a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddError {
    /// No buffers.
    Empty,
    /// Fewer free descriptors than buffers.
    Full,
}

/// A used-ring element the driver will not act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsedError {
    /// The id is not a descriptor index of this queue.
    BadId(u32),
    /// The id is a descriptor, but not the head of a chain in flight.
    NotInFlight(u16),
    /// [`SplitQueue::pop_used_expect`]: the next element is for another request.
    /// It is left in place.
    WrongToken { expected: u16, got: u32 },
}

/// The driver's state for one split virtqueue of `Q` entries.
pub struct SplitQueue<const Q: usize> {
    /// Free-list links, and within an in-flight chain the chain's own links.
    next: [u16; Q],
    /// Descriptors in the chain headed at `i`; 0 = `i` heads nothing in flight.
    chain: [u16; Q],
    /// First free descriptor; `Q` when none is free.
    free_head: u16,
    num_free: u16,
    /// Shadow of `avail.idx`: the next slot the driver fills.
    avail_idx: u16,
    /// The next used-ring slot the driver has not consumed.
    last_used: u16,
}

impl<const Q: usize> Default for SplitQueue<Q> {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::cast_possible_truncation)] // Q <= 32768, asserted in `new`
impl<const Q: usize> SplitQueue<Q> {
    pub const LAYOUT: Layout = Layout::new(Q);

    const AVAIL_FLAGS: usize = Self::LAYOUT.avail / 2;
    const AVAIL_IDX: usize = Self::AVAIL_FLAGS + 1;
    const AVAIL_RING: usize = Self::AVAIL_FLAGS + 2;
    const USED_FLAGS: usize = Self::LAYOUT.used / 2;
    const USED_IDX: usize = Self::USED_FLAGS + 1;
    const USED_RING: usize = Self::USED_FLAGS + 2;

    /// Every descriptor free, both indices at 0: the state after a device reset.
    #[must_use]
    pub const fn new() -> Self {
        assert!(Q.is_power_of_two() && Q <= 32768, "a split queue's size is a power of two <= 32768");
        let mut next = [0u16; Q];
        let mut i = 0;
        while i < Q {
            next[i] = (i + 1) as u16;
            i += 1;
        }
        Self { next, chain: [0; Q], free_head: 0, num_free: Q as u16, avail_idx: 0, last_used: 0 }
    }

    /// Zero the queue's memory and forget every request. Before the queue's
    /// addresses are given to the device, and only then.
    pub fn reset(&mut self, m: &impl QueueMem) {
        for w in 0..Self::LAYOUT.words() {
            m.write(w, 0);
        }
        *self = Self::new();
    }

    #[must_use]
    pub const fn num_free(&self) -> usize {
        self.num_free as usize
    }

    /// Whether `head` is the head of a request the device has not completed.
    #[must_use]
    pub fn in_flight(&self, head: u16) -> bool {
        (head as usize) < Q && self.chain[head as usize] != 0
    }

    /// Post one request: its buffers become a descriptor chain, its head goes
    /// into the available ring, and `avail.idx` is published. Returns the head,
    /// which is the request's token in the used ring.
    ///
    /// The caller must then notify the device (if [`Self::should_notify`]) and
    /// keep every buffer alive and unmoved until the token comes back.
    pub fn add(&mut self, m: &impl QueueMem, bufs: &[Buf]) -> Result<u16, AddError> {
        if bufs.is_empty() {
            return Err(AddError::Empty);
        }
        if bufs.len() > self.num_free as usize {
            return Err(AddError::Full);
        }
        let head = self.free_head;
        let mut cur = head;
        for (k, b) in bufs.iter().enumerate() {
            let last = k + 1 == bufs.len();
            let after = self.next[cur as usize];
            let mut flags = if b.device_writes { DESC_F_WRITE } else { 0 };
            if !last {
                flags |= DESC_F_NEXT;
            }
            let w = Self::LAYOUT.desc / 2 + 8 * cur as usize;
            write_u64(m, w, b.addr);
            write_u32(m, w + 4, b.len);
            m.write(w + 6, flags);
            m.write(w + 7, if last { 0 } else { after });
            if last {
                self.free_head = after;
            } else {
                cur = after;
            }
        }
        self.num_free -= bufs.len() as u16;
        self.chain[head as usize] = bufs.len() as u16;

        m.write(Self::AVAIL_RING + self.avail_idx as usize % Q, head);
        // The descriptors and the ring slot before the index that publishes them.
        fence(Ordering::SeqCst);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        m.write(Self::AVAIL_IDX, self.avail_idx);
        // The index before the notify (and before reading `used.flags`).
        fence(Ordering::SeqCst);
        Ok(head)
    }

    /// Whether the device wants a notify for what was just added.
    #[must_use]
    pub fn should_notify(&self, m: &impl QueueMem) -> bool {
        m.read(Self::USED_FLAGS) & USED_F_NO_NOTIFY == 0
    }

    /// Whether the device has completed a request the driver has not consumed.
    #[must_use]
    pub fn has_used(&self, m: &impl QueueMem) -> bool {
        m.read(Self::USED_IDX) != self.last_used
    }

    /// The id of the next completed request, without consuming it — exactly
    /// as the device wrote it, which [`Self::pop_used`] then validates.
    #[must_use]
    pub fn peek_used(&self, m: &impl QueueMem) -> Option<u32> {
        if !self.has_used(m) {
            return None;
        }
        // The element after the index that published it.
        fence(Ordering::SeqCst);
        Some(read_u32(m, Self::used_elem(self.last_used)))
    }

    /// Consume the next completed request: `(head, bytes the device wrote)`.
    ///
    /// The element is consumed even when it is refused, so a device that
    /// completes garbage cannot wedge the queue on it; a refused element frees
    /// nothing.
    pub fn pop_used(&mut self, m: &impl QueueMem) -> Option<Result<(u16, u32), UsedError>> {
        if !self.has_used(m) {
            return None;
        }
        fence(Ordering::SeqCst);
        let e = Self::used_elem(self.last_used);
        let (id, len) = (read_u32(m, e), read_u32(m, e + 2));
        self.last_used = self.last_used.wrapping_add(1);
        Some(self.release(id).map(|head| (head, len)))
    }

    /// [`Self::pop_used`] for one particular request: the next element must be
    /// `token`'s, or it is left in place and reported as
    /// [`UsedError::WrongToken`]. Returns the bytes the device wrote.
    pub fn pop_used_expect(&mut self, m: &impl QueueMem, token: u16) -> Option<Result<u32, UsedError>> {
        let got = self.peek_used(m)?;
        if got != u32::from(token) {
            return Some(Err(UsedError::WrongToken { expected: token, got }));
        }
        self.pop_used(m).map(|r| r.map(|(_, len)| len))
    }

    const fn used_elem(idx: u16) -> usize {
        Self::USED_RING + 4 * (idx as usize % Q)
    }

    /// Return the chain headed at `id` to the free list.
    fn release(&mut self, id: u32) -> Result<u16, UsedError> {
        if id as usize >= Q {
            return Err(UsedError::BadId(id));
        }
        let head = id as u16;
        let n = self.chain[head as usize];
        if n == 0 {
            return Err(UsedError::NotInFlight(head));
        }
        let mut tail = head;
        for _ in 1..n {
            tail = self.next[tail as usize];
        }
        self.next[tail as usize] = self.free_head;
        self.free_head = head;
        self.num_free += n;
        self.chain[head as usize] = 0;
        Ok(head)
    }
}

fn write_u32(m: &impl QueueMem, w: usize, v: u32) {
    m.write(w, v as u16);
    m.write(w + 1, (v >> 16) as u16);
}

fn write_u64(m: &impl QueueMem, w: usize, v: u64) {
    write_u32(m, w, v as u32);
    write_u32(m, w + 2, (v >> 32) as u32);
}

fn read_u32(m: &impl QueueMem, w: usize) -> u32 {
    u32::from(m.read(w)) | u32::from(m.read(w + 1)) << 16
}

#[cfg(test)]
mod tests;

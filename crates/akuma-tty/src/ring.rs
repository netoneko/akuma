//! A fixed-capacity FIFO, allocated once.
//!
//! The storage is claimed in [`Ring::try_new`] — fallibly, so a pty open under
//! memory pressure is an `ENOMEM` rather than an allocator abort — and never
//! grows afterwards. Every operation on the read and write paths is index
//! arithmetic over that one buffer: nothing a user can do to a terminal makes
//! the kernel allocate.

use alloc::vec::Vec;

#[derive(Debug)]
pub struct Ring<T> {
    buf: Vec<T>,
    head: usize,
    len: usize,
}

impl<T: Copy + Default> Ring<T> {
    /// A ring holding up to `capacity` entries, or `None` if the storage
    /// cannot be allocated (or `capacity` is zero, which no caller wants and
    /// which would make every index computation a division by zero).
    #[must_use]
    pub fn try_new(capacity: usize) -> Option<Self> {
        if capacity == 0 {
            return None;
        }
        let mut buf = Vec::new();
        buf.try_reserve_exact(capacity).ok()?;
        // Within the reservation just made: this fills, it does not reallocate.
        buf.resize(capacity, T::default());
        Some(Self { buf, head: 0, len: 0 })
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub fn room(&self) -> usize {
        self.capacity() - self.len
    }

    /// Append one entry; `false` (and nothing stored) when full.
    pub fn push(&mut self, v: T) -> bool {
        if self.len == self.capacity() {
            return false;
        }
        let i = (self.head + self.len) % self.capacity();
        self.buf[i] = v;
        self.len += 1;
        true
    }

    /// Remove and return the oldest entry.
    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let v = self.buf[self.head];
        self.head = (self.head + 1) % self.capacity();
        self.len -= 1;
        Some(v)
    }

    /// The newest entry, for marking it after the fact.
    pub fn last_mut(&mut self) -> Option<&mut T> {
        if self.len == 0 {
            return None;
        }
        let i = (self.head + self.len - 1) % self.capacity();
        Some(&mut self.buf[i])
    }

    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// Oldest to newest, without consuming.
    pub fn iter(&self) -> impl Iterator<Item = T> + '_ {
        let cap = self.capacity();
        (0..self.len).map(move |i| self.buf[(self.head + i) % cap])
    }

    /// Rewrite the contents in place: each entry is passed to `f`, which keeps
    /// it (possibly changed) or drops it. Order is preserved. O(len), no
    /// allocation — the ring is rotated through itself.
    pub fn retain_map(&mut self, mut f: impl FnMut(T) -> Option<T>) {
        let n = self.len;
        for _ in 0..n {
            let Some(v) = self.pop() else { break };
            if let Some(v) = f(v) {
                // Room is guaranteed: one entry was just popped.
                self.push(v);
            }
        }
    }
}

impl Ring<u8> {
    /// Move up to `out.len()` bytes out, oldest first.
    pub fn read_into(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.len);
        let cap = self.capacity();
        let first = n.min(cap - self.head);
        out[..first].copy_from_slice(&self.buf[self.head..self.head + first]);
        out[first..n].copy_from_slice(&self.buf[..n - first]);
        self.head = (self.head + n) % cap;
        self.len -= n;
        n
    }
}

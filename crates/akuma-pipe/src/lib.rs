//! The pure byte-FIFO half of a kernel pipe.
//!
//! A [`Pipe`] is a bounded `VecDeque<u8>` with one **closable write end**.
//!
//! With it come the two rules that make `read(2)`/`write(2)` on opposite sides
//! of it behave:
//!
//! * a write to a full pipe takes nothing and a partial write takes what fits —
//!   the caller retries the rest (`sshd`'s bridge carries the residue across
//!   ticks);
//! * a read of an empty pipe is [`ReadOutcome::WouldBlock`] while the writer is
//!   open and [`ReadOutcome::Eof`] once it has closed.
//!
//! # What is deliberately not here
//!
//! No lock, no global table, no poller/waker registry, no syscall numbers, no
//! `fd`. Those belong to whatever integrates this — on the amd64 target that is
//! `amd64/src/pipe.rs`, a `spin`-locked fixed array of `Pipe`s indexed by a
//! `PipeId`. The tree's full kernel pipe
//! (`akuma_syscalls_glue::pipe::KernelPipe`) has the same buffer/cap/short-write
//! shape and additionally a `pollers` map for waking parked readers; that crate
//! does not build for `x86_64-unknown-none` (it is behind `akuma-exec` and
//! reaches AArch64 `global_asm!`), which is why this leaf exists. If a
//! `forbid(unsafe_code)` pipe is ever wanted on the AArch64 side too, the buffer
//! half is here to share.
//!
//! `#![forbid(unsafe_code)]`: it is a `VecDeque` and two booleans.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::VecDeque;

/// Linux's default pipe capacity, 64 KiB.
///
/// Matches `akuma_syscalls_glue::pipe::MAX_PIPE_CAPACITY`. A write past this
/// point is truncated to the space available rather than growing the buffer
/// without bound — the failure mode that unbounded growth caused on the AArch64
/// side is recorded in that module.
pub const DEFAULT_CAPACITY: usize = 64 * 1024;

/// What a [`Pipe::read`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    /// Bytes were copied out. Always `1..=out.len()`.
    Read(usize),
    /// The buffer is empty but the write end is still open — poll again.
    WouldBlock,
    /// The buffer is drained and the write end is closed: end of file.
    Eof,
}

/// One pipe: a bounded FIFO with a closable producer end.
#[derive(Debug)]
pub struct Pipe {
    buf: VecDeque<u8>,
    capacity: usize,
    write_closed: bool,
}

impl Pipe {
    /// A pipe with [`DEFAULT_CAPACITY`].
    #[must_use]
    pub const fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// A pipe with an explicit byte cap. `capacity` is clamped to at least 1 so
    /// a write can always make progress once the buffer drains.
    ///
    /// `const` so an integrator can hold `Pipe`s in a `static` fixed array
    /// without an initialiser function (`amd64/src/pipe.rs`).
    #[must_use]
    pub const fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            capacity: if capacity == 0 { 1 } else { capacity },
            write_closed: false,
        }
    }

    /// Reset to the empty, open state, keeping the allocation. For a fixed-array
    /// integration reusing a slot.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.write_closed = false;
    }

    /// Bytes currently buffered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Is the buffer empty?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Room for another write, in bytes.
    #[must_use]
    pub fn room(&self) -> usize {
        self.capacity - self.buf.len()
    }

    /// Has the producer closed its end?
    #[must_use]
    pub fn is_write_closed(&self) -> bool {
        self.write_closed
    }

    /// Should a consumer act on this pipe now — data waiting, or EOF to report?
    #[must_use]
    pub fn is_readable(&self) -> bool {
        !self.buf.is_empty() || self.write_closed
    }

    /// Append up to [`Self::room`] bytes. Returns how many were taken; a short
    /// count (including 0) means the buffer is full and the caller retries the
    /// rest. Writing to a closed pipe still returns 0 rather than erroring — the
    /// caller learns the consumer is gone from the count, and `SIGPIPE` is not a
    /// concept this half models.
    pub fn write(&mut self, data: &[u8]) -> usize {
        let n = self.room().min(data.len());
        self.buf.extend(&data[..n]);
        n
    }

    /// Copy out up to `out.len()` bytes. See [`ReadOutcome`].
    pub fn read(&mut self, out: &mut [u8]) -> ReadOutcome {
        if self.buf.is_empty() {
            return if self.write_closed {
                ReadOutcome::Eof
            } else {
                ReadOutcome::WouldBlock
            };
        }
        let n = out.len().min(self.buf.len());
        for slot in out.iter_mut().take(n) {
            // `pop_front` is `Some` for every one of the `n` iterations because
            // `n <= self.buf.len()` and nothing else drains it here.
            *slot = self.buf.pop_front().unwrap_or(0);
        }
        ReadOutcome::Read(n)
    }

    /// Close the producer end. Idempotent. A consumer that has drained the
    /// buffer then sees [`ReadOutcome::Eof`] instead of blocking forever.
    pub fn close_write(&mut self) {
        self.write_closed = true;
    }
}

impl Default for Pipe {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_round_trips() {
        let mut p = Pipe::new();
        assert_eq!(p.write(b"hello"), 5);
        let mut out = [0u8; 8];
        assert_eq!(p.read(&mut out), ReadOutcome::Read(5));
        assert_eq!(&out[..5], b"hello");
    }

    #[test]
    fn empty_open_pipe_would_block_empty_closed_pipe_is_eof() {
        let mut p = Pipe::new();
        let mut out = [0u8; 4];
        assert_eq!(p.read(&mut out), ReadOutcome::WouldBlock);
        p.close_write();
        assert_eq!(p.read(&mut out), ReadOutcome::Eof);
    }

    #[test]
    fn buffered_bytes_drain_before_eof_is_reported() {
        let mut p = Pipe::new();
        p.write(b"ab");
        p.close_write();
        let mut out = [0u8; 1];
        assert_eq!(p.read(&mut out), ReadOutcome::Read(1));
        assert_eq!(p.read(&mut out), ReadOutcome::Read(1));
        assert_eq!(p.read(&mut out), ReadOutcome::Eof);
    }

    #[test]
    fn write_past_capacity_is_a_short_write_not_growth() {
        let mut p = Pipe::with_capacity(4);
        assert_eq!(p.write(b"abcdef"), 4);
        assert_eq!(p.write(b"g"), 0);
        assert_eq!(p.len(), 4);
        let mut out = [0u8; 2];
        assert_eq!(p.read(&mut out), ReadOutcome::Read(2));
        assert_eq!(p.write(b"gh"), 2);
    }

    #[test]
    fn partial_read_leaves_the_rest() {
        let mut p = Pipe::new();
        p.write(b"abcde");
        let mut out = [0u8; 2];
        assert_eq!(p.read(&mut out), ReadOutcome::Read(2));
        assert_eq!(&out, b"ab");
        let mut rest = [0u8; 8];
        assert_eq!(p.read(&mut rest), ReadOutcome::Read(3));
        assert_eq!(&rest[..3], b"cde");
    }

    #[test]
    fn clear_returns_a_slot_to_the_empty_open_state() {
        let mut p = Pipe::with_capacity(8);
        p.write(b"data");
        p.close_write();
        p.clear();
        assert!(p.is_empty());
        assert!(!p.is_write_closed());
        assert_eq!(p.room(), 8);
    }
}

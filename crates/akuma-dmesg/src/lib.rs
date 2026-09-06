//! The kernel console history ring behind `dmesg(1)`, plus the `syslog(2)`
//! action decode.
//!
//! Every byte the kernel writes to its console is also written here, so a
//! diagnostic that has scrolled off — or that was printed before anything was
//! attached to read it — can still be recovered later by a userspace `dmesg`.
//!
//! # Why this is a crate
//!
//! It started as `amd64/src/serial.rs`'s `KLOG`: a `static mut [u8; 64 * 1024]`
//! with a `u64` write counter, four raw-pointer helpers around it, and
//! `sys_syslog` on top. The bare-metal reference machine's console is a
//! write-only television with no scrollback, so `dmesg` over ssh is the *only*
//! way to read a boot on it.
//!
//! Three reasons it does not belong in a UART driver:
//!
//! 1. **The wrap/skip arithmetic is the entire substance, and it was already
//!    wrong once.** `sys_syslog` read the ring in a single pass bounded by its
//!    4 KiB staging buffer, while `SYSLOG_ACTION_SIZE_BUFFER` truthfully
//!    advertised the full 64 KiB. `busybox dmesg` allocated 64 KiB, asked for
//!    64 KiB, and got the last 4 KiB back with nothing reporting a short read —
//!    so fifteen sixteenths of every boot log was unreachable on the one
//!    machine that needed it. An xHCI bring-up printed its whole trace and the
//!    NIC's stall dumps then pushed it out of reach. That is a pure-function
//!    bug, and [`Ring::snapshot_from`]'s tests below cost a millisecond to run.
//! 2. **`static mut` plus raw-pointer indexing**, in a tree whose stated
//!    direction is `#![forbid(unsafe_code)]` across `src/`. A ring that owns its
//!    array and takes `&mut self` needs none of it.
//! 3. **AArch64 has no `dmesg` at all.** There is no `syslog(2)` on that side,
//!    so every kernel diagnostic older than the serial scrollback is simply
//!    gone. The ring was written for the target that could not live without it;
//!    the other one merely never noticed it was missing.
//!
//! # This crate is not `akuma_kernel_core::klog`
//!
//! That module is the **`log` crate sink** — it routes `log::info!` from
//! `akuma-net` and smoltcp into the console. This is the console *history*.
//! They sit on opposite ends of the same pipe and share no code. The naming
//! collision is why this crate is `akuma-dmesg` and not `akuma-klog`.
//!
//! # What is deliberately not here
//!
//! No lock, no `static`, no UART, no user-memory copy, no syscall number. A
//! [`Ring`] is a plain value; the kernel that owns one decides what serialises
//! access to it — on amd64 that is `serial.rs`'s existing best-effort console
//! lock, which every console byte already passes under. Keeping the lock out is
//! what keeps the `forbid` honest and what lets the same type be tested on the
//! host with no ceremony at all.
//!
//! # Optional means `CAP = 0`
//!
//! A build that does not want to spend the `.bss` selects `Ring<0>`: the
//! buffer is a zero-length array, `push` compiles to nothing, and every read
//! answers "empty". The type is not itself zero-sized — the `u64` counter
//! stays, so it is **8 bytes rather than `CAP`** ([`Ring::capacity`] is what
//! reports the difference, and `zero_capacity_ring_costs_only_the_counter`
//! pins it). That is a type-level choice rather than a `cfg` threaded through
//! every call site, so the disabled build still type-checks the code it is not
//! keeping. `CAP == 0` is guarded explicitly and tested, because `% CAP` on
//! that path would otherwise be a division by zero.

#![no_std]
#![forbid(unsafe_code)]

/// A fixed-capacity ring of the most recent `CAP` console bytes.
///
/// Bytes are pushed one at a time and never fail: once `CAP` bytes are in, each
/// new byte evicts the oldest. `total` counts every byte ever pushed, so
/// [`Ring::dropped`] can report how much history was lost — the number a caller
/// needs to know its `dmesg` is not the whole story.
///
/// `CAP == 0` is a valid, fully-functional degenerate ring that stores nothing.
/// See the module header.
#[derive(Clone, Copy)]
pub struct Ring<const CAP: usize> {
    buf: [u8; CAP],
    /// Total bytes ever pushed, not bytes held. The write cursor is
    /// `total % CAP`; the oldest retrievable byte is at absolute index
    /// `total - len()`.
    ///
    /// `u64` rather than `usize` so the wrap is unreachable rather than merely
    /// unlikely: at one byte per nanosecond it is ~585 years.
    total: u64,
}

impl<const CAP: usize> Default for Ring<CAP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const CAP: usize> Ring<CAP> {
    /// An empty ring. `const`, so it can initialise a `static` directly.
    #[must_use]
    pub const fn new() -> Self {
        Self { buf: [0; CAP], total: 0 }
    }

    /// The ring's capacity in bytes. `0` means history is disabled.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        CAP
    }

    /// Append one byte, evicting the oldest if the ring is full.
    ///
    /// A no-op at `CAP == 0` — including the counter, which stays at zero so
    /// [`Ring::dropped`] does not report a loss the caller could never have
    /// avoided.
    pub const fn push(&mut self, byte: u8) {
        if CAP == 0 {
            return;
        }
        self.buf[(self.total % CAP as u64) as usize] = byte;
        self.total += 1;
    }

    /// Append every byte of `bytes`.
    pub fn push_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.push(b);
        }
    }

    /// Append `s`, expanding each `\n` to `\r\n`.
    ///
    /// The ring holds what a terminal was shown, and a console driver that
    /// emits the CR itself (amd64's `putb`) hands both bytes to [`push`] on the
    /// way past. A caller that only has the `&str` — a diagnostic routed to the
    /// history *without* being printed, so a high-frequency ticker does not
    /// scroll a television somebody is watching — needs the same expansion, or
    /// its lines stairstep when `dmesg` replays them.
    ///
    /// [`push`]: Ring::push
    pub fn push_str_crlf(&mut self, s: &str) {
        for &b in s.as_bytes() {
            if b == b'\n' {
                self.push(b'\r');
            }
            self.push(b);
        }
    }

    /// How many bytes are currently retrievable — `min(total, CAP)`.
    ///
    /// This is the answer for both `SYSLOG_ACTION_SIZE_UNREAD` and
    /// `SYSLOG_ACTION_SIZE_BUFFER`: this ring has no read cursor, so everything
    /// held is always "unread".
    #[must_use]
    pub const fn len(&self) -> usize {
        let total = self.total;
        if total < CAP as u64 { total as usize } else { CAP }
    }

    /// Whether any history is retrievable.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total bytes ever pushed, including those since evicted.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// How many bytes have been evicted and are gone for good.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.total - self.len() as u64
    }

    /// Copy history into `out`, starting `skip` bytes past the oldest byte still
    /// retrievable. Returns how many bytes were written.
    ///
    /// `skip == 0` is the oldest retrievable byte, so successive calls with an
    /// advancing `skip` walk *forward* through history and a caller with a small
    /// staging buffer can still deliver the whole ring a chunk at a time. That
    /// is the parameter whose absence made a 4 KiB staging buffer a hard ceiling
    /// on a 64 KiB `dmesg` — see the module header.
    ///
    /// A `skip` past the end returns `0` rather than wrapping, so the natural
    /// `while n != 0` drain loop terminates.
    #[must_use]
    pub fn snapshot_from(&self, skip: usize, out: &mut [u8]) -> usize {
        let available = self.len();
        let Some(remaining) = available.checked_sub(skip) else {
            return 0;
        };
        let want = remaining.min(out.len());
        if want == 0 {
            return 0;
        }
        // Absolute index of the oldest retrievable byte, then `skip` past it.
        // `dropped()` is exact because `total` counts pushes, not wraps.
        let start = self.dropped() + skip as u64;
        for (i, slot) in out[..want].iter_mut().enumerate() {
            *slot = self.buf[((start + i as u64) % CAP as u64) as usize];
        }
        want
    }

    /// Discard all history. For `SYSLOG_ACTION_CLEAR`.
    ///
    /// Resets `total` too, so a cleared ring reports `dropped() == 0`: after a
    /// deliberate clear there is no loss to report, only a fresh start.
    pub const fn clear(&mut self) {
        self.total = 0;
    }
}

/// One `syslog(2)` action, decoded from the raw first argument.
///
/// Both kernels' `sys_syslog` decode the same numbers, and the numbers are the
/// part where a typo is invisible: an action mapped to the wrong arm makes
/// `dmesg` clear the log instead of reading it, and nothing reports an error.
/// `Close`/`Open`/`ConsoleOff`/`ConsoleOn`/`ConsoleLevel` are accepted and
/// answered `0` — this kernel has no console-level machinery, and failing them
/// makes `busybox dmesg` print an error before doing the thing that works.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// 0 — `SYSLOG_ACTION_CLOSE`. Accepted, no effect.
    Close,
    /// 1 — `SYSLOG_ACTION_OPEN`. Accepted, no effect.
    Open,
    /// 2 — `SYSLOG_ACTION_READ`. Read into the caller's buffer.
    Read,
    /// 3 — `SYSLOG_ACTION_READ_ALL`. What `busybox dmesg` uses.
    ReadAll,
    /// 4 — `SYSLOG_ACTION_READ_CLEAR`. `dmesg -c`.
    ReadClear,
    /// 5 — `SYSLOG_ACTION_CLEAR`. `dmesg -C`.
    Clear,
    /// 6 — `SYSLOG_ACTION_CONSOLE_OFF`. Accepted, no effect.
    ConsoleOff,
    /// 7 — `SYSLOG_ACTION_CONSOLE_ON`. Accepted, no effect.
    ConsoleOn,
    /// 8 — `SYSLOG_ACTION_CONSOLE_LEVEL`. Accepted, no effect.
    ConsoleLevel,
    /// 9 — `SYSLOG_ACTION_SIZE_UNREAD`. Answer with [`Ring::len`].
    SizeUnread,
    /// 10 — `SYSLOG_ACTION_SIZE_BUFFER`. Answer with [`Ring::len`].
    ///
    /// Linux answers with the ring's *capacity* here, not its fill. This kernel
    /// answers with the fill deliberately: `busybox dmesg` allocates whatever
    /// this returns and then asks for exactly that many bytes, so advertising a
    /// 64 KiB capacity against a 3 KiB log makes it request 64 KiB and read
    /// short — which is how the original truncation bug presented. **Pinned
    /// divergence**, not an oversight.
    SizeBuffer,
}

impl Action {
    /// Decode the raw `syslog(2)` action number. `None` is `EINVAL`.
    #[must_use]
    pub const fn decode(action: u64) -> Option<Self> {
        Some(match action {
            0 => Self::Close,
            1 => Self::Open,
            2 => Self::Read,
            3 => Self::ReadAll,
            4 => Self::ReadClear,
            5 => Self::Clear,
            6 => Self::ConsoleOff,
            7 => Self::ConsoleOn,
            8 => Self::ConsoleLevel,
            9 => Self::SizeUnread,
            10 => Self::SizeBuffer,
            _ => return None,
        })
    }

    /// Does this action copy history into the caller's buffer?
    #[must_use]
    pub const fn reads(self) -> bool {
        matches!(self, Self::Read | Self::ReadAll | Self::ReadClear)
    }

    /// Does this action discard history once it is done?
    #[must_use]
    pub const fn clears(self) -> bool {
        matches!(self, Self::Clear | Self::ReadClear)
    }

    /// Does this action answer with a size rather than bytes?
    #[must_use]
    pub const fn sizes(self) -> bool {
        matches!(self, Self::SizeUnread | Self::SizeBuffer)
    }

    /// Is this action accepted with no effect and a `0` result?
    #[must_use]
    pub const fn is_noop(self) -> bool {
        matches!(
            self,
            Self::Close | Self::Open | Self::ConsoleOff | Self::ConsoleOn | Self::ConsoleLevel
        )
    }
}

#[cfg(test)]
mod tests;

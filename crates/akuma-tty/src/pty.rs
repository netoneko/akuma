//! Unix98 pseudo-terminals (`/dev/ptmx` + `/dev/pts/N`) — one pair's state and
//! every rule about it, host-tested.
//!
//! A terminal emulator (rio, `script`, `expect`, tmux, an `sshd`) opens the
//! **master**, gets a pair number, opens the **slave** by that number and runs a
//! shell on it. Bytes the emulator writes to the master are *keyboard input*:
//! they go through the line discipline (ICRNL, canonical editing, echo, ISIG)
//! and become what the shell reads from the slave. Bytes the shell writes to the
//! slave are *screen output*: they go through output processing (ONLCR) and
//! become what the emulator reads from the master. That is the whole device; the
//! rest is termios, the window size, and who gets which signal.
//!
//! [`PtyPair`] is that device with the kernel taken out:
//!
//! - **Two fixed buffers**, allocated once when the pair is created
//!   ([`PtyPair::try_new`], fallible) and never grown: input (master → slave,
//!   after the line discipline) and output (slave → master, after output
//!   processing). A full buffer is backpressure — a short write — never an
//!   allocation and never silent loss of data a writer was told was accepted.
//! - **The N_TTY line discipline**: [`Termios`] in the kernel's 36-byte wire
//!   layout, canonical lines with erase/kill/word-erase/literal-next, EOF as a
//!   zero-length line, ECHO/ECHOE/ECHOK/ECHOKE/ECHOCTL/ECHONL, ICRNL/INLCR/IGNCR,
//!   ISTRIP, IUTF8-aware erase, ISIG.
//! - **Read decisions**, including the non-canonical `VMIN`/`VTIME` matrix —
//!   [`PtyPair::slave_read`] answers "data / EOF / block, and for how long" and
//!   the kernel owns the clock.
//! - **Readiness** for `poll`/`epoll` ([`PtyPair::master_poll`],
//!   [`PtyPair::slave_poll`]) and the **hangup rules**: the master reads `EIO`
//!   and polls `POLLHUP` once every slave descriptor is gone; the slave reads EOF
//!   and writes `EIO` once the master is.
//!
//! Effects are **returned, never performed** — the `akuma-pipes` shape, for the
//! same reasons. Signals a keystroke raises come back as [`Signals`] for the
//! caller to deliver after dropping its lock (delivery can run a default
//! terminate action inline, which closes descriptors, which takes this lock);
//! waiters come back as [`Wakes`].
//!
//! What stays in the kernel: the pair table and its lock, the descriptors and
//! their reference counts' *callers*, `O_NONBLOCK`, parking and the `VTIME`
//! deadline, process groups and sessions, and every user copy.
//!
//! # Divergences from Linux, pinned
//!
//! - **No output flow control.** `IXON`'s `^S`/`^Q` are ordinary data.
//! - **No packet mode** (`TIOCPKT`), no `VREPRINT`/`VDISCARD`, no `ECHOPRT`,
//!   no break/parity handling. Erasing a tab erases one column.
//! - **`IUTF8` is on in the initial termios.** Linux leaves it to the emulator;
//!   every emulator that runs here speaks UTF-8, and without it erasing a
//!   multi-byte character leaves a broken sequence in the line.
//! - **Writing the master after the slave hung up is `EIO`** rather than
//!   queueing bytes nobody can read — a writer that ignores hangup would
//!   otherwise fill the input buffer and block forever.

pub use crate::ld::{INPUT_CAPACITY, LineDiscipline, OUTPUT_CAPACITY, Signals};
pub use crate::termios::{MAX_CANON, Termios, Transition, Winsize, cc, flags, ioctl, sig};

/// The `/dev/pts/N` number space. A policy, not a buffer: the kernel's table is
/// this many pointers, and a pair's storage exists only while it is open.
pub const MAX_PTYS: usize = 64;
/// Threads that can wait on one pair at once.
///
/// Readers, writers and pollers of both sides together. A waiter that finds the
/// set full must park with a short timeout instead of indefinitely — see
/// [`PtyPair::register_waiter`].
pub const MAX_WAITERS: usize = 8;




/// Threads to make runnable, as `(tid, token)`. Fixed size, so taking it out of
/// a pair under a spinlock allocates nothing.
#[derive(Debug)]
pub struct Wakes<W> {
    slots: [Option<(usize, W)>; MAX_WAITERS],
}

impl<W: Copy> Wakes<W> {
    #[must_use]
    pub const fn none() -> Self {
        Self { slots: [None; MAX_WAITERS] }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }

    /// Fire each token. Call this **after** releasing the lock the pair is in.
    pub fn fire(self, mut f: impl FnMut(usize, W)) {
        for (tid, w) in self.slots.into_iter().flatten() {
            f(tid, w);
        }
    }
}

/// What a master read found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterRead {
    Data(usize),
    /// Nothing queued, slave still (or not yet) open: block, or `EAGAIN`.
    WouldBlock,
    /// Nothing queued and every slave descriptor is closed: `EIO`, Linux's
    /// "the program on the terminal is gone".
    Hangup,
}

/// What a slave read found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlaveRead {
    /// Return this many bytes — **possibly 0**: an EOF line, or a `VMIN=0` read
    /// that found nothing (with `VTIME=0`, or after its timer ran out).
    Data(usize),
    /// The master is gone: the terminal hung up, reads are EOF.
    Hangup,
    /// Nothing returnable yet. `timeout_ds` is the `VTIME` timer to arm, in
    /// tenths of a second, when the read should give up waiting; `None` is an
    /// untimed wait. Pass `expired = true` on the retry after the timer fires.
    Block { timeout_ds: Option<u8> },
}

/// What one master write did to the input side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Input {
    /// Bytes consumed. Fewer than offered means the input buffer is full:
    /// block or `EAGAIN` for the rest.
    pub accepted: usize,
    /// Signals the line discipline raised (`ISIG`), for the foreground group.
    pub signals: Signals,
}

/// The other side is gone; the operation is `EIO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HungUp;

/// Readiness of one side, before any masking by what was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Poll {
    pub readable: bool,
    pub writable: bool,
    pub hup: bool,
}

/// Why the slave cannot be opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlaveOpenError {
    /// `unlockpt` has not been called (`TIOCSPTLCK` with 0).
    Locked,
    /// The master is closed.
    MasterGone,
}

/// One master/slave pair.
///
/// The four `bool`s are four independent facts (literal-next pending, locked,
/// slave ever opened, changed since the last wake hand-out); folding them into
/// a state enum would invent combinations that do not exist.
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct PtyPair<W> {
    /// The slave's termios. Termios requests on the master act on the slave,
    /// as Linux's `tty_mode_ioctl` does, so there is one.
    termios: Termios,
    pub winsize: Winsize,
    /// Foreground process group of the terminal (`TIOCSPGRP`), 0 for none.
    pub fg_pgid: u32,
    /// Session leader this terminal controls (`TIOCSCTTY`), 0 for none.
    pub session: u32,

    /// The line discipline's buffers and editing state; the termios above is
    /// passed to it on every call, so there is one copy of the flags.
    ld: LineDiscipline,

    locked: bool,
    master_refs: u32,
    slave_refs: u32,
    /// The slave has been opened at least once. A master whose slave was never
    /// opened is waiting for it, not hung up.
    slave_opened: bool,

    waiters: [Option<(usize, W)>; MAX_WAITERS],
    /// Something a waiter could be waiting for has happened since the last
    /// [`PtyPair::take_wakes`]. Registering a waiter does **not** set it — the
    /// call that registers a blocked reader must not hand that same reader its
    /// own wake back, or it spins instead of sleeping.
    changed: bool,
}

impl<W: Copy> PtyPair<W> {
    /// A fresh pair, as `open("/dev/ptmx")` makes it: one master reference,
    /// locked, no slave, 24x80. `None` when the buffers cannot be allocated.
    #[must_use]
    pub fn try_new() -> Option<Self> {
        Some(Self {
            termios: Termios::initial(),
            winsize: Winsize { row: 24, col: 80, xpixel: 0, ypixel: 0 },
            fg_pgid: 0,
            session: 0,
            ld: LineDiscipline::try_new()?,
            locked: true,
            master_refs: 1,
            slave_refs: 0,
            slave_opened: false,
            waiters: [None; MAX_WAITERS],
            changed: false,
        })
    }

    // ---- references -------------------------------------------------------

    /// `unlockpt` / `TIOCSPTLCK`.
    pub fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
        self.changed = true;
    }

    #[must_use]
    pub const fn is_locked(&self) -> bool {
        self.locked
    }

    /// One more master descriptor (`dup`, `fork`).
    pub fn master_ref(&mut self) {
        self.master_refs += 1;
    }

    /// One more slave descriptor from a `dup`/`fork` — not an open, so no lock
    /// or hangup check.
    pub fn slave_ref(&mut self) {
        self.slave_refs += 1;
    }

    /// `open("/dev/pts/N")` (or `TIOCGPTPEER`, `/dev/tty` in its session).
    pub fn slave_open(&mut self) -> Result<(), SlaveOpenError> {
        if self.master_refs == 0 {
            return Err(SlaveOpenError::MasterGone);
        }
        if self.locked {
            return Err(SlaveOpenError::Locked);
        }
        self.slave_refs += 1;
        self.slave_opened = true;
        self.changed = true;
        Ok(())
    }

    /// Drop one master reference. Returns `true` when this was the last one —
    /// the terminal has hung up and the caller sends `SIGHUP` to the session.
    pub fn master_close(&mut self) -> bool {
        self.master_refs = self.master_refs.saturating_sub(1);
        self.changed = true;
        if self.master_refs == 0 {
            // Nothing can read what the slave writes any more, and the slave
            // reads EOF from here on: neither buffer has a consumer.
            self.flush(true, true);
            return true;
        }
        false
    }

    /// Drop one slave reference.
    pub fn slave_close(&mut self) {
        self.slave_refs = self.slave_refs.saturating_sub(1);
        self.changed = true;
    }

    /// No descriptor names either side: the pair can be freed.
    #[must_use]
    pub const fn is_unreferenced(&self) -> bool {
        self.master_refs == 0 && self.slave_refs == 0
    }

    /// The slave side hung up: it was opened and every descriptor is closed.
    #[must_use]
    pub const fn master_hup(&self) -> bool {
        self.slave_opened && self.slave_refs == 0
    }

    /// The master side is gone.
    #[must_use]
    pub const fn slave_hup(&self) -> bool {
        self.master_refs == 0
    }

    // ---- termios, window size, queues -------------------------------------

    #[must_use]
    pub const fn termios(&self) -> Termios {
        self.termios
    }

    /// `TCSETS*`. Switching canonical mode off makes the half-edited line
    /// readable as raw bytes; switching it on makes whatever raw bytes are
    /// queued readable as one line — neither loses input a reader was owed.
    pub fn set_termios(&mut self, t: Termios) {
        let was = self.termios.canonical();
        self.termios = t;
        self.ld.termios_changed(was, &t);
        // `VMIN`/`ICANON` decide readability, so a parked reader re-tests.
        self.changed = true;
    }

    /// `TIOCSWINSZ`. `true` when the size changed — the caller then sends
    /// `SIGWINCH` to the foreground group; Linux does not signal a no-op set.
    pub fn set_winsize(&mut self, ws: Winsize) -> bool {
        let changed = self.winsize != ws;
        self.winsize = ws;
        self.changed |= changed;
        changed
    }

    /// `TCFLSH` / `TCSETSF`, and the `ISIG` flush.
    pub fn flush(&mut self, input: bool, output: bool) {
        self.changed = true;
        self.ld.flush(input, output);
    }

    /// Bytes a slave read could return (`FIONREAD` on the slave).
    #[must_use]
    pub fn input_queued(&self) -> usize {
        self.ld.input_queued()
    }

    /// Bytes a master read could return (`FIONREAD` on the master, `TIOCOUTQ`
    /// on the slave).
    #[must_use]
    pub const fn output_queued(&self) -> usize {
        self.ld.output_queued()
    }

    // ---- readiness ----------------------------------------------------------

    #[must_use]
    pub fn master_poll(&self) -> Poll {
        let hup = self.master_hup();
        Poll {
            readable: !self.ld.output_is_empty() || hup,
            writable: !hup && self.ld.can_accept_input(&self.termios),
            hup,
        }
    }

    #[must_use]
    pub fn slave_poll(&self) -> Poll {
        let hup = self.slave_hup();
        Poll { readable: hup || self.ld.readable(&self.termios), writable: hup || self.ld.output_has_room(), hup }
    }

    // ---- waiters ------------------------------------------------------------

    /// Register `tid` to be woken on the next state change of this pair.
    /// Re-registering replaces the token. `false` when the set is full: the
    /// caller is **not** registered and must wait with a short timeout.
    pub fn register_waiter(&mut self, tid: usize, token: W) -> bool {
        if let Some(slot) = self.waiters.iter_mut().find(|s| s.is_some_and(|(t, _)| t == tid)) {
            *slot = Some((tid, token));
            return true;
        }
        if let Some(slot) = self.waiters.iter_mut().find(|s| s.is_none()) {
            *slot = Some((tid, token));
            return true;
        }
        false
    }

    /// Every registered waiter, removed — **if anything changed** since the
    /// last call, and nothing otherwise. Call after every operation and fire
    /// the result once the lock is dropped; each woken thread re-tests its own
    /// condition and re-registers if it still has to wait. An operation that
    /// only looked (a read that found nothing, a poll) leaves the waiters —
    /// including one it just registered — in place.
    pub fn take_wakes(&mut self) -> Wakes<W> {
        let mut w = Wakes::none();
        if self.changed {
            self.changed = false;
            core::mem::swap(&mut w.slots, &mut self.waiters);
        }
        w
    }

    // ---- the master side ----------------------------------------------------

    /// Read what the slave wrote (plus echo).
    pub fn master_read(&mut self, out: &mut [u8]) -> MasterRead {
        if !self.ld.output_is_empty() {
            let n = self.ld.read_output(out);
            self.changed |= n > 0;
            return MasterRead::Data(n);
        }
        if self.master_hup() { MasterRead::Hangup } else { MasterRead::WouldBlock }
    }

    /// Type `data` at the terminal: run it through the line discipline.
    pub fn master_write(&mut self, data: &[u8]) -> Result<Input, HungUp> {
        if self.master_hup() {
            return Err(HungUp);
        }
        let mut signals = Signals::default();
        let accepted = self.ld.receive_all(&self.termios, data, &mut signals);
        self.changed |= accepted > 0;
        Ok(Input { accepted, signals })
    }

    // ---- the slave side -----------------------------------------------------

    /// Write program output to the screen.
    pub fn slave_write(&mut self, data: &[u8]) -> Result<usize, HungUp> {
        if self.slave_hup() {
            return Err(HungUp);
        }
        let n = self.ld.write_all(&self.termios, data);
        self.changed |= n > 0;
        Ok(n)
    }

    /// Read keyboard input. `expired` is "the `VTIME` timer this read armed has
    /// run out".
    pub fn slave_read(&mut self, out: &mut [u8], expired: bool) -> SlaveRead {
        if self.slave_hup() {
            return SlaveRead::Hangup;
        }
        let before = self.ld.input_len();
        let r = self.ld.read_input(&self.termios, out, expired);
        // Draining makes room a blocked master writer is waiting for.
        self.changed |= self.ld.input_len() != before;
        r
    }
}

#[cfg(test)]
mod tests;

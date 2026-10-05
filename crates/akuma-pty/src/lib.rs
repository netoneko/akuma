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

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

mod ring;

pub use ring::Ring;

use alloc::vec::Vec;

/// The `ioctl(2)` request numbers this device answers. asm-generic values,
/// which x86_64 shares.
pub mod ioctl {
    pub const TCGETS: u32 = 0x5401;
    pub const TCSETS: u32 = 0x5402;
    pub const TCSETSW: u32 = 0x5403;
    pub const TCSETSF: u32 = 0x5404;
    pub const TCSBRK: u32 = 0x5409;
    pub const TCXONC: u32 = 0x540A;
    pub const TCFLSH: u32 = 0x540B;
    pub const TIOCSCTTY: u32 = 0x540E;
    pub const TIOCGPGRP: u32 = 0x540F;
    pub const TIOCSPGRP: u32 = 0x5410;
    pub const TIOCOUTQ: u32 = 0x5411;
    pub const TIOCGWINSZ: u32 = 0x5413;
    pub const TIOCSWINSZ: u32 = 0x5414;
    pub const FIONREAD: u32 = 0x541B;
    pub const TIOCPKT: u32 = 0x5420;
    pub const TIOCNOTTY: u32 = 0x5422;
    pub const TIOCGSID: u32 = 0x5429;
    pub const TCSBRKP: u32 = 0x5425;
    /// `_IOR('T', 0x30, unsigned int)` — the pair number, for `ptsname`.
    pub const TIOCGPTN: u32 = 0x8004_5430;
    /// `_IOW('T', 0x31, int)` — `unlockpt` writes 0 here.
    pub const TIOCSPTLCK: u32 = 0x4004_5431;
    /// `_IOR('T', 0x39, int)`.
    pub const TIOCGPTLCK: u32 = 0x8004_5439;
    /// `_IO('T', 0x41)` — open the slave straight from a master descriptor.
    pub const TIOCGPTPEER: u32 = 0x5441;

    /// `TCFLSH` arguments.
    pub const TCIFLUSH: u64 = 0;
    pub const TCOFLUSH: u64 = 1;
    pub const TCIOFLUSH: u64 = 2;
}

/// termios flag bits (asm-generic `termbits.h`; octal, as Linux spells them).
pub mod flags {
    // c_iflag
    pub const ISTRIP: u32 = 0o40;
    pub const INLCR: u32 = 0o100;
    pub const IGNCR: u32 = 0o200;
    pub const ICRNL: u32 = 0o400;
    pub const IXON: u32 = 0o2000;
    pub const IUTF8: u32 = 0o40000;
    // c_oflag
    pub const OPOST: u32 = 0o1;
    pub const ONLCR: u32 = 0o4;
    pub const OCRNL: u32 = 0o10;
    // c_cflag
    pub const B38400: u32 = 0o17;
    pub const CS8: u32 = 0o60;
    pub const CREAD: u32 = 0o200;
    // c_lflag
    pub const ISIG: u32 = 0o1;
    pub const ICANON: u32 = 0o2;
    pub const ECHO: u32 = 0o10;
    pub const ECHOE: u32 = 0o20;
    pub const ECHOK: u32 = 0o40;
    pub const ECHONL: u32 = 0o100;
    pub const NOFLSH: u32 = 0o200;
    pub const ECHOCTL: u32 = 0o1000;
    pub const ECHOKE: u32 = 0o4000;
    pub const IEXTEN: u32 = 0o100000;
}

/// `c_cc` indices.
pub mod cc {
    pub const VINTR: usize = 0;
    pub const VQUIT: usize = 1;
    pub const VERASE: usize = 2;
    pub const VKILL: usize = 3;
    pub const VEOF: usize = 4;
    pub const VTIME: usize = 5;
    pub const VMIN: usize = 6;
    pub const VSTART: usize = 8;
    pub const VSTOP: usize = 9;
    pub const VSUSP: usize = 10;
    pub const VEOL: usize = 11;
    pub const VREPRINT: usize = 12;
    pub const VDISCARD: usize = 13;
    pub const VWERASE: usize = 14;
    pub const VLNEXT: usize = 15;
    pub const VEOL2: usize = 16;
    /// Length of the kernel's `c_cc` (musl's userspace array is 32; the
    /// kernel copies 19).
    pub const NCCS: usize = 19;
}

/// The signals a terminal raises.
pub mod sig {
    pub const SIGHUP: u32 = 1;
    pub const SIGINT: u32 = 2;
    pub const SIGQUIT: u32 = 3;
    pub const SIGCONT: u32 = 18;
    pub const SIGTSTP: u32 = 20;
    pub const SIGWINCH: u32 = 28;
}

use cc::*;
use flags::*;

/// The `/dev/pts/N` number space. A policy, not a buffer: the kernel's table is
/// this many pointers, and a pair's storage exists only while it is open.
pub const MAX_PTYS: usize = 64;
/// Master → slave buffer, in entries.
///
/// Large enough that a full canonical line ([`MAX_CANON`] plus its terminator)
/// always fits once the slave has drained, which is what keeps a long line from
/// wedging the writer.
pub const INPUT_CAPACITY: usize = 8192;
/// Slave → master buffer, in bytes.
pub const OUTPUT_CAPACITY: usize = 8192;
/// Longest canonical line, matching Linux's N_TTY.
pub const MAX_CANON: usize = 4095;
/// Threads that can wait on one pair at once.
///
/// Readers, writers and pollers of both sides together. A waiter that finds the
/// set full must park with a short timeout instead of indefinitely — see
/// [`PtyPair::register_waiter`].
pub const MAX_WAITERS: usize = 8;

/// `_POSIX_VDISABLE`: a `c_cc` slot holding 0 is switched off.
const DISABLED: u8 = 0;

/// Input entries carry the byte in the low 8 bits plus these flags.
///
/// `DELIM` ends a canonical line (a canonical read stops after it); `EOF_MARK`
/// is the zero-length line `^D` on an empty line produces — it terminates a
/// read without being copied, so a read that meets it first returns 0.
const DELIM: u16 = 0x100;
const EOF_MARK: u16 = 0x200;

/// The kernel `struct termios` (not musl's larger userspace one): four flag
/// words, `c_line`, and 19 control characters — 36 bytes on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Termios {
    pub iflag: u32,
    pub oflag: u32,
    pub cflag: u32,
    pub lflag: u32,
    pub line: u8,
    pub cc: [u8; NCCS],
}

impl Termios {
    pub const WIRE_LEN: usize = 36;

    /// Linux's `tty_std_termios` with `INIT_C_CC`, as a pty slave starts — plus
    /// `IUTF8` (see the crate header).
    #[must_use]
    pub const fn initial() -> Self {
        let mut cc = [0u8; NCCS];
        cc[VINTR] = 0x03;
        cc[VQUIT] = 0x1C;
        cc[VERASE] = 0x7F;
        cc[VKILL] = 0x15;
        cc[VEOF] = 0x04;
        cc[VTIME] = 0;
        cc[VMIN] = 1;
        cc[VSTART] = 0x11;
        cc[VSTOP] = 0x13;
        cc[VSUSP] = 0x1A;
        cc[VREPRINT] = 0x12;
        cc[VDISCARD] = 0x0F;
        cc[VWERASE] = 0x17;
        cc[VLNEXT] = 0x16;
        Self {
            iflag: ICRNL | IXON | IUTF8,
            oflag: OPOST | ONLCR,
            cflag: B38400 | CS8 | CREAD,
            lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN,
            line: 0,
            cc,
        }
    }

    /// `cfmakeraw`: what a terminal client (the Akuma `ssh`'s
    /// `SET_TERMINAL_ATTRIBUTES` raw-mode request) wants of the tty it reads
    /// keys from — no line editing, no echo, no signal keys, no CR/NL or
    /// output translation, `read` returns as soon as one byte is there.
    pub fn make_raw(&mut self) {
        self.iflag &= !(ISTRIP | INLCR | IGNCR | ICRNL | IXON);
        self.oflag &= !OPOST;
        self.lflag &= !(ECHO | ECHOE | ECHOK | ECHONL | ECHOCTL | ECHOKE | ICANON | ISIG | IEXTEN);
        self.cc[VMIN] = 1;
        self.cc[VTIME] = 0;
    }

    /// The inverse of [`Self::make_raw`]: the flags a pty slave starts with.
    /// (The pair keeps no saved copy, so settings the program had changed
    /// before going raw are not restored — only the defaults.)
    pub fn make_cooked(&mut self) {
        let sane = Self::initial();
        self.iflag |= sane.iflag & (ICRNL | IXON);
        self.oflag |= sane.oflag;
        self.lflag |= sane.lflag;
    }

    /// The wire image `TCGETS` copies out. `c_cc[0]` is at byte **17**: byte
    /// 16 is `c_line`, which is not part of the array.
    #[must_use]
    pub fn to_wire(&self) -> [u8; Self::WIRE_LEN] {
        let mut w = [0u8; Self::WIRE_LEN];
        w[0..4].copy_from_slice(&self.iflag.to_le_bytes());
        w[4..8].copy_from_slice(&self.oflag.to_le_bytes());
        w[8..12].copy_from_slice(&self.cflag.to_le_bytes());
        w[12..16].copy_from_slice(&self.lflag.to_le_bytes());
        w[16] = self.line;
        w[17..].copy_from_slice(&self.cc);
        w
    }

    #[must_use]
    pub fn from_wire(w: &[u8; Self::WIRE_LEN]) -> Self {
        let word = |o: usize| u32::from_le_bytes([w[o], w[o + 1], w[o + 2], w[o + 3]]);
        let mut cc = [0u8; NCCS];
        cc.copy_from_slice(&w[17..]);
        Self { iflag: word(0), oflag: word(4), cflag: word(8), lflag: word(12), line: w[16], cc }
    }

    #[must_use]
    pub const fn canonical(&self) -> bool {
        self.lflag & ICANON != 0
    }

    /// Is `c` the (enabled) control character in slot `idx`?
    const fn is(&self, c: u8, idx: usize) -> bool {
        self.cc[idx] != DISABLED && self.cc[idx] == c
    }
}

impl Default for Termios {
    fn default() -> Self {
        Self::initial()
    }
}

/// `struct winsize` — 8 bytes on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Winsize {
    pub row: u16,
    pub col: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

impl Winsize {
    pub const WIRE_LEN: usize = 8;

    #[must_use]
    pub fn to_wire(&self) -> [u8; Self::WIRE_LEN] {
        let mut w = [0u8; Self::WIRE_LEN];
        w[0..2].copy_from_slice(&self.row.to_le_bytes());
        w[2..4].copy_from_slice(&self.col.to_le_bytes());
        w[4..6].copy_from_slice(&self.xpixel.to_le_bytes());
        w[6..8].copy_from_slice(&self.ypixel.to_le_bytes());
        w
    }

    #[must_use]
    pub const fn from_wire(w: &[u8; Self::WIRE_LEN]) -> Self {
        Self {
            row: u16::from_le_bytes([w[0], w[1]]),
            col: u16::from_le_bytes([w[2], w[3]]),
            xpixel: u16::from_le_bytes([w[4], w[5]]),
            ypixel: u16::from_le_bytes([w[6], w[7]]),
        }
    }
}

/// A set of signal numbers below 32, raised by input processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Signals(u32);

impl Signals {
    pub fn add(&mut self, sig: u32) {
        if sig < 32 {
            self.0 |= 1 << sig;
        }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn contains(&self, sig: u32) -> bool {
        sig < 32 && self.0 & (1 << sig) != 0
    }

    /// The signals, lowest number first.
    pub fn iter(self) -> impl Iterator<Item = u32> {
        (1..32u32).filter(move |&s| self.0 & (1 << s) != 0)
    }
}

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

    /// Bytes the slave can read, after the line discipline, with line flags.
    input: Ring<u16>,
    /// How many `DELIM` entries `input` holds — complete canonical lines.
    delims: usize,
    /// The canonical line being edited. Capacity [`MAX_CANON`], reserved once.
    line: Vec<u8>,
    /// `VLNEXT` was the last input byte: take the next one literally.
    lnext: bool,
    /// Bytes the master can read, after output processing (and echo).
    output: Ring<u8>,

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
        let mut line = Vec::new();
        line.try_reserve_exact(MAX_CANON).ok()?;
        Some(Self {
            termios: Termios::initial(),
            winsize: Winsize { row: 24, col: 80, xpixel: 0, ypixel: 0 },
            fg_pgid: 0,
            session: 0,
            input: Ring::try_new(INPUT_CAPACITY)?,
            delims: 0,
            line,
            lnext: false,
            output: Ring::try_new(OUTPUT_CAPACITY)?,
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
        self.lnext = false;
        // `VMIN`/`ICANON` decide readability, so a parked reader re-tests.
        self.changed = true;
        if was && !t.canonical() {
            for i in 0..self.line.len() {
                if !self.input.push(u16::from(self.line[i])) {
                    break;
                }
            }
            self.line.clear();
            // Raw reads ignore line structure: drop EOF marks, clear the flags.
            self.input.retain_map(|e| if e & EOF_MARK != 0 { None } else { Some(e & 0xFF) });
            self.delims = 0;
        } else if !was && t.canonical()
            && let Some(last) = self.input.last_mut()
        {
            *last |= DELIM;
            self.delims = 1;
        }
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
        if input {
            self.input.clear();
            self.delims = 0;
            self.line.clear();
            self.lnext = false;
        }
        if output {
            self.output.clear();
        }
    }

    /// Bytes a slave read could return (`FIONREAD` on the slave).
    #[must_use]
    pub fn input_queued(&self) -> usize {
        self.input.iter().filter(|e| e & EOF_MARK == 0).count()
    }

    /// Bytes a master read could return (`FIONREAD` on the master, `TIOCOUTQ`
    /// on the slave).
    #[must_use]
    pub const fn output_queued(&self) -> usize {
        self.output.len()
    }

    // ---- readiness ----------------------------------------------------------

    #[must_use]
    pub fn master_poll(&self) -> Poll {
        let hup = self.master_hup();
        Poll { readable: !self.output.is_empty() || hup, writable: !hup && self.can_accept_input(), hup }
    }

    #[must_use]
    pub fn slave_poll(&self) -> Poll {
        let hup = self.slave_hup();
        let readable = hup
            || if self.termios.canonical() {
                self.delims > 0
            } else {
                // Linux `n_tty_poll`: with `VTIME` set one byte is enough; without
                // it, `VMIN` bytes (at least one).
                let need = if self.termios.cc[VTIME] != 0 { 1 } else { usize::from(self.termios.cc[VMIN]).max(1) };
                self.input.len() >= need
            };
        Poll { readable, writable: hup || self.output.room() >= 2, hup }
    }

    /// Would a master write consume at least one byte?
    fn can_accept_input(&self) -> bool {
        if self.termios.canonical() {
            // A line terminator needs the whole line to fit.
            self.input.room() > self.line.len()
        } else {
            self.input.room() > 0
        }
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
        if !self.output.is_empty() {
            let n = self.output.read_into(out);
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
        let mut accepted = 0;
        for &c in data {
            if !self.receive(c, &mut signals) {
                break;
            }
            accepted += 1;
        }
        self.changed |= accepted > 0;
        Ok(Input { accepted, signals })
    }

    // ---- the slave side -----------------------------------------------------

    /// Write program output to the screen.
    pub fn slave_write(&mut self, data: &[u8]) -> Result<usize, HungUp> {
        if self.slave_hup() {
            return Err(HungUp);
        }
        let mut n = 0;
        for &c in data {
            if !self.out_char(c) {
                break;
            }
            n += 1;
        }
        self.changed |= n > 0;
        Ok(n)
    }

    /// Read keyboard input. `expired` is "the `VTIME` timer this read armed has
    /// run out".
    pub fn slave_read(&mut self, out: &mut [u8], expired: bool) -> SlaveRead {
        let before = self.input.len();
        let r = self.slave_read_inner(out, expired);
        // Draining makes room a blocked master writer is waiting for.
        self.changed |= self.input.len() != before;
        r
    }

    fn slave_read_inner(&mut self, out: &mut [u8], expired: bool) -> SlaveRead {
        if self.slave_hup() {
            return SlaveRead::Hangup;
        }
        if out.is_empty() {
            return SlaveRead::Data(0);
        }
        if self.termios.canonical() {
            if self.delims == 0 {
                return SlaveRead::Block { timeout_ds: None };
            }
            // One line per read, at most `out.len()` of it; the rest of a long
            // line stays for the next read.
            let mut n = 0;
            while n < out.len() {
                let Some(e) = self.input.pop() else { break };
                if e & DELIM != 0 {
                    self.delims -= 1;
                    if e & EOF_MARK == 0 {
                        out[n] = e as u8;
                        n += 1;
                    }
                    break;
                }
                out[n] = e as u8;
                n += 1;
            }
            return SlaveRead::Data(n);
        }

        let vmin = usize::from(self.termios.cc[VMIN]);
        let vtime = self.termios.cc[VTIME];
        let avail = self.input.len();
        let ready = if vmin == 0 {
            avail > 0 || vtime == 0 || expired
        } else {
            avail >= vmin.min(out.len()) || (expired && avail > 0)
        };
        if ready {
            let mut n = 0;
            while n < out.len() {
                let Some(e) = self.input.pop() else { break };
                out[n] = e as u8;
                n += 1;
            }
            return SlaveRead::Data(n);
        }
        // `VMIN>0, VTIME>0`: the timer is inter-byte and starts at the first
        // byte, so with nothing queued the wait is untimed.
        let timeout_ds = if vtime == 0 || (vmin > 0 && avail == 0) { None } else { Some(vtime) };
        SlaveRead::Block { timeout_ds }
    }

    // ---- the line discipline ------------------------------------------------

    /// One input byte. `false` when it could not be consumed for lack of room
    /// (backpressure: the master write is short).
    fn receive(&mut self, c: u8, signals: &mut Signals) -> bool {
        let t = self.termios;

        if self.lnext {
            self.lnext = false;
            return self.store_literal(c);
        }

        let mut c = c;
        if t.iflag & ISTRIP != 0 {
            c &= 0x7F;
        }
        if c == b'\r' {
            if t.iflag & IGNCR != 0 {
                return true;
            }
            if t.iflag & ICRNL != 0 {
                c = b'\n';
            }
        } else if c == b'\n' && t.iflag & INLCR != 0 {
            c = b'\r';
        }

        if t.lflag & IEXTEN != 0 && t.is(c, VLNEXT) {
            self.lnext = true;
            if t.lflag & ECHO != 0 && t.lflag & ECHOCTL != 0 {
                // Linux echoes `^` and backs over it, so the next character
                // lands on top.
                self.out_raw(b"^\x08");
            }
            return true;
        }

        if t.lflag & ISIG != 0 {
            let sig = if t.is(c, VINTR) {
                sig::SIGINT
            } else if t.is(c, VQUIT) {
                sig::SIGQUIT
            } else if t.is(c, VSUSP) {
                sig::SIGTSTP
            } else {
                0
            };
            if sig != 0 {
                if t.lflag & NOFLSH == 0 {
                    self.flush(true, true);
                }
                if t.lflag & ECHO != 0 {
                    self.echo(c);
                }
                signals.add(sig);
                return true;
            }
        }

        if !t.canonical() {
            if !self.input.push(u16::from(c)) {
                return false;
            }
            if t.lflag & ECHO != 0 {
                self.echo(c);
            }
            return true;
        }

        if t.is(c, VERASE) {
            self.erase_char();
            return true;
        }
        if t.is(c, VKILL) {
            self.kill_line(c);
            return true;
        }
        if t.lflag & IEXTEN != 0 && t.is(c, VWERASE) {
            self.erase_word();
            return true;
        }
        if t.is(c, VEOF) {
            // The line so far becomes readable without the EOF character; on
            // an empty line that is a zero-length read — EOF.
            return self.finish_line(EOF_MARK | DELIM);
        }
        if c == b'\n' || t.is(c, VEOL) || (t.lflag & IEXTEN != 0 && t.is(c, VEOL2)) {
            if self.input.room() <= self.line.len() {
                return false;
            }
            if t.lflag & ECHO != 0 || (c == b'\n' && t.lflag & ECHONL != 0) {
                self.echo(c);
            }
            return self.finish_line(u16::from(c) | DELIM);
        }
        if self.line.len() < MAX_CANON {
            // Within the capacity reserved in `try_new`: never reallocates.
            self.line.push(c);
            if t.lflag & ECHO != 0 {
                self.echo(c);
            }
        }
        // A full line drops the byte (Linux rings the bell); it was still
        // consumed, so the writer is not blocked on a line that can only end.
        true
    }

    /// The byte after `VLNEXT`: data, whatever it is.
    fn store_literal(&mut self, c: u8) -> bool {
        if self.termios.canonical() {
            if self.line.len() < MAX_CANON {
                self.line.push(c);
            }
        } else if !self.input.push(u16::from(c)) {
            self.lnext = true;
            return false;
        }
        if self.termios.lflag & ECHO != 0 {
            self.echo(c);
        }
        true
    }

    /// Move the edited line, then `terminator`, into the readable queue.
    fn finish_line(&mut self, terminator: u16) -> bool {
        if self.input.room() <= self.line.len() {
            return false;
        }
        for i in 0..self.line.len() {
            self.input.push(u16::from(self.line[i]));
        }
        self.input.push(terminator);
        self.delims += 1;
        self.line.clear();
        true
    }

    /// `VERASE`: remove one character — a whole UTF-8 sequence under `IUTF8` —
    /// and rub it out on screen.
    fn erase_char(&mut self) {
        let t = self.termios;
        let Some(c) = self.line.pop() else { return };
        let is_cont = |b: u8| b & 0xC0 == 0x80;
        if t.iflag & IUTF8 != 0 && is_cont(c) {
            while let Some(b) = self.line.pop() {
                if !is_cont(b) {
                    break;
                }
            }
        }
        if t.lflag & ECHO == 0 {
            return;
        }
        if t.lflag & ECHOE != 0 {
            // A control character was echoed as two columns (`^X`).
            let width = if t.lflag & ECHOCTL != 0 && is_ctl(c) { 2 } else { 1 };
            for _ in 0..width {
                self.out_raw(b"\x08 \x08");
            }
        } else {
            self.echo(t.cc[VERASE]);
        }
    }

    /// `VWERASE`: trailing blanks, then the word before them.
    fn erase_word(&mut self) {
        while self.line.last().is_some_and(|&b| b == b' ' || b == b'\t') {
            self.erase_char();
        }
        while self.line.last().is_some_and(|&b| b != b' ' && b != b'\t') {
            self.erase_char();
        }
    }

    /// `VKILL`: the whole line. Linux rubs it out character by character only
    /// with `ECHOE|ECHOK|ECHOKE` all set (the default); otherwise it echoes the
    /// kill character, and a newline if `ECHOK`.
    fn kill_line(&mut self, kill_char: u8) {
        let lflag = self.termios.lflag;
        if lflag & ECHO == 0 {
            self.line.clear();
            return;
        }
        if lflag & ECHOK == 0 || lflag & ECHOKE == 0 || lflag & ECHOE == 0 {
            self.line.clear();
            self.echo(kill_char);
            if lflag & ECHOK != 0 {
                self.out_char(b'\n');
            }
            return;
        }
        while !self.line.is_empty() {
            self.erase_char();
        }
    }

    /// Echo one input byte: control characters as `^X` under `ECHOCTL`
    /// (except tab and newline), everything through output processing.
    /// Dropped when the output buffer is full, as Linux drops echo.
    fn echo(&mut self, c: u8) {
        if self.termios.lflag & ECHOCTL != 0 && is_ctl(c) {
            self.out_raw(&[b'^', c ^ 0x40]);
        } else {
            self.out_char(c);
        }
    }

    /// Queue `bytes` for the master whole, or not at all.
    fn out_raw(&mut self, bytes: &[u8]) -> bool {
        if self.output.room() < bytes.len() {
            return false;
        }
        for &b in bytes {
            self.output.push(b);
        }
        true
    }

    /// One byte of output through `OPOST`. `false` for lack of room.
    fn out_char(&mut self, c: u8) -> bool {
        let o = self.termios.oflag;
        if o & OPOST != 0 {
            if c == b'\n' && o & ONLCR != 0 {
                return self.out_raw(b"\r\n");
            }
            if c == b'\r' && o & OCRNL != 0 {
                return self.output.push(b'\n');
            }
        }
        self.output.push(c)
    }
}

/// Echoed as `^X` under `ECHOCTL`.
const fn is_ctl(c: u8) -> bool {
    (c < 0x20 && c != b'\t' && c != b'\n') || c == 0x7F
}

#[cfg(test)]
mod tests;

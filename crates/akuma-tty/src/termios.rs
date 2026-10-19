//! The termios vocabulary both line disciplines in this crate speak.
//!
//! Ioctl request numbers, flag bits, `c_cc` indices, the kernel `struct termios`
//! / `struct winsize` wire layouts, and the [`Transition`] a mode change reports.
//!
//! One copy. `session::TerminalState` and `pty::PtyPair` used to spell the same
//! flag bits twice (hex in one, octal in the other) and three kernels' `TCGETS`
//! spelled the 36-byte layout by hand.

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

/// Longest canonical line, matching Linux's N_TTY.
pub const MAX_CANON: usize = 4095;

/// `_POSIX_VDISABLE`: a `c_cc` slot holding 0 is switched off.
pub(crate) const DISABLED: u8 = 0;

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

    /// What a console or ssh-session terminal starts as: Linux's
    /// `tty_std_termios` — the same as a pty slave ([`Self::initial`]).
    ///
    /// Until 2026-10-19 this was the amd64 console's compiled-in literals
    /// (`0x500 / 5 / 0xBF / 0x8A3B`, only `VINTR`/`VQUIT`/`VERASE`/`VKILL`/`VEOF`/
    /// `VMIN`/`VSUSP` set, no `IUTF8`) and the AArch64 terminal started somewhere
    /// else again (`cflag 0`, no `ECHOCTL`/`ECHOKE`/`IEXTEN`). With one line
    /// discipline underneath, a console that left `VWERASE`/`VLNEXT`/`IUTF8` off
    /// just made `^W`, `^V` and multi-byte erase behave differently from a pty.
    #[must_use]
    pub const fn console_default() -> Self {
        Self::initial()
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
    pub(crate) const fn is(&self, c: u8, idx: usize) -> bool {
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

/// What a termios change did to the raw/canonical bit (`ICANON`).
///
/// Every function that can change a terminal's mode returns one, so the edge
/// that matters — [`Transition::Cooked`], a raw terminal going back to line
/// mode — is a value a caller must look at rather than a before/after compare
/// each call site re-derives (`5a577983`: three copies of it, one trace).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Transition {
    /// Canonical stayed canonical, or raw stayed raw.
    Same,
    /// Canonical -> raw.
    Raw,
    /// Raw -> canonical: input typed while raw is now sitting in a line
    /// buffer waiting for a newline a raw-mode program never sends.
    Cooked,
}

impl Transition {
    pub const fn between(was_canonical: bool, now_canonical: bool) -> Self {
        match (was_canonical, now_canonical) {
            (true, false) => Self::Raw,
            (false, true) => Self::Cooked,
            _ => Self::Same,
        }
    }

    #[must_use]
    pub const fn is_cooked(self) -> bool {
        matches!(self, Self::Cooked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_default_is_the_pty_default() {
        let t = Termios::console_default();
        assert_eq!(t, Termios::initial());
        assert_eq!((t.iflag, t.oflag, t.cflag, t.lflag), (0x4500, 0x5, 0xBF, 0x8A3B));
        for (idx, want) in [
            (cc::VINTR, 0x03),
            (cc::VQUIT, 0x1C),
            (cc::VERASE, 0x7F),
            (cc::VKILL, 0x15),
            (cc::VEOF, 0x04),
            (cc::VSTART, 0x11),
            (cc::VSTOP, 0x13),
            (cc::VSUSP, 0x1A),
            (cc::VREPRINT, 0x12),
            (cc::VDISCARD, 0x0F),
            (cc::VWERASE, 0x17),
            (cc::VLNEXT, 0x16),
        ] {
            assert_eq!(t.cc[idx], want, "c_cc[{idx}]");
        }
        assert_eq!((t.cc[cc::VMIN], t.cc[cc::VTIME]), (1, 0));
    }

    #[test]
    fn wire_layout_puts_cc_at_byte_17() {
        let t = Termios::console_default();
        let w = t.to_wire();
        assert_eq!(w[16], 0);
        assert_eq!(w[17 + cc::VINTR], 3);
        assert_eq!(Termios::from_wire(&w), t);
    }

    #[test]
    fn transition_between() {
        assert_eq!(Transition::between(true, true), Transition::Same);
        assert_eq!(Transition::between(false, false), Transition::Same);
        assert_eq!(Transition::between(true, false), Transition::Raw);
        assert!(Transition::between(false, true).is_cooked());
    }
}

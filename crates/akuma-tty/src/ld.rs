//! The N_TTY line discipline as a standalone engine.
//!
//! [`LineDiscipline`] is what a terminal does with bytes: input processing
//! (ICRNL/INLCR/IGNCR/ISTRIP, canonical editing, echo, `ISIG`, literal-next),
//! output processing (ONLCR/OCRNL), the readable queue and the `VMIN`/`VTIME`
//! read decision. It owns the **buffers** and nothing else — the termios is a
//! parameter of every call, so whoever owns the flags (a [`crate::pty::PtyPair`],
//! a [`crate::session::TerminalState`]) stays their single owner and a mode
//! change cannot be seen by the engine late.
//!
//! No locks, no waiters, no hangup rules, no refcounts: those are the callers'.
//! Effects are returned ([`Signals`]), never performed. Storage is two fixed
//! rings and one line buffer, reserved once in [`LineDiscipline::try_new`]
//! (fallible) and never grown.

use alloc::vec::Vec;

use crate::pty::SlaveRead;
use crate::ring::Ring;
use crate::termios::{MAX_CANON, Termios, cc::*, flags::*, sig};

/// Master → slave buffer, in entries.
///
/// Large enough that a full canonical line ([`MAX_CANON`] plus its terminator)
/// always fits once the slave has drained, which is what keeps a long line from
/// wedging the writer.
pub const INPUT_CAPACITY: usize = 8192;
/// Slave → master buffer, in bytes.
pub const OUTPUT_CAPACITY: usize = 8192;
/// Input entries carry the byte in the low 8 bits plus these flags.
///
/// `DELIM` ends a canonical line (a canonical read stops after it); `EOF_MARK`
/// is the zero-length line `^D` on an empty line produces — it terminates a
/// read without being copied, so a read that meets it first returns 0.
const DELIM: u16 = 0x100;
const EOF_MARK: u16 = 0x200;

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

/// The buffers and editing state of one terminal line discipline.
#[derive(Debug)]
pub struct LineDiscipline {
    /// Bytes a reader can take, after the discipline, with line flags.
    input: Ring<u16>,
    /// How many `DELIM` entries `input` holds — complete canonical lines.
    delims: usize,
    /// The canonical line being edited. Capacity [`MAX_CANON`], reserved once.
    line: Vec<u8>,
    /// `VLNEXT` was the last input byte: take the next one literally.
    lnext: bool,
    /// Bytes the other side (the master, or the screen) can take: output after
    /// OPOST, and echo.
    output: Ring<u8>,
}

impl LineDiscipline {
    /// Empty buffers. `None` when the storage cannot be allocated.
    #[must_use]
    pub fn try_new() -> Option<Self> {
        let mut line = Vec::new();
        line.try_reserve_exact(MAX_CANON).ok()?;
        Some(Self {
            input: Ring::try_new(INPUT_CAPACITY)?,
            delims: 0,
            line,
            lnext: false,
            output: Ring::try_new(OUTPUT_CAPACITY)?,
        })
    }

    /// The mode changed from `was_canonical` to `t`. Switching canonical mode
    /// off makes the half-edited line readable as raw bytes; switching it on
    /// makes whatever raw bytes are queued readable as one line — neither loses
    /// input a reader was owed.
    pub fn termios_changed(&mut self, was_canonical: bool, t: &Termios) {
        self.lnext = false;
        if was_canonical && !t.canonical() {
            for i in 0..self.line.len() {
                if !self.input.push(u16::from(self.line[i])) {
                    break;
                }
            }
            self.line.clear();
            // Raw reads ignore line structure: drop EOF marks, clear the flags.
            self.input.retain_map(|e| if e & EOF_MARK != 0 { None } else { Some(e & 0xFF) });
            self.delims = 0;
        } else if !was_canonical && t.canonical()
            && let Some(last) = self.input.last_mut()
        {
            *last |= DELIM;
            self.delims = 1;
        }
    }

    /// `TCFLSH` / `TCSETSF`, and the `ISIG` flush.
    pub fn flush(&mut self, input: bool, output: bool) {
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

    /// Bytes a read could return (`FIONREAD`).
    #[must_use]
    pub fn input_queued(&self) -> usize {
        self.input.iter().filter(|e| e & EOF_MARK == 0).count()
    }

    /// A completed line is waiting.
    #[must_use]
    pub const fn has_line(&self) -> bool {
        self.delims > 0
    }

    /// Text is being edited and has no terminator yet.
    #[must_use]
    pub const fn has_partial_line(&self) -> bool {
        !self.line.is_empty()
    }

    /// Make the half-edited line readable as it stands: it ends at the next
    /// read like a `^D`-terminated one (no terminator byte, no EOF).
    pub fn flush_line(&mut self) -> bool {
        if self.line.is_empty() {
            return false;
        }
        self.finish_line(EOF_MARK | DELIM)
    }

    /// Entries in the readable queue (any change means a reader made progress).
    #[must_use]
    pub const fn input_len(&self) -> usize {
        self.input.len()
    }

    /// Bytes waiting for the other side (`FIONREAD` on the master, `TIOCOUTQ`).
    #[must_use]
    pub const fn output_queued(&self) -> usize {
        self.output.len()
    }

    /// Is there anything for the other side to read?
    #[must_use]
    pub const fn output_is_empty(&self) -> bool {
        self.output.is_empty()
    }

    /// Take up to `out.len()` bytes of output/echo.
    pub fn read_output(&mut self, out: &mut [u8]) -> usize {
        self.output.read_into(out)
    }

    /// Room for two output bytes (one `\n` expands to two).
    #[must_use]
    pub fn output_has_room(&self) -> bool {
        self.output.room() >= 2
    }

    /// Would an input byte be consumed?
    #[must_use]
    pub fn can_accept_input(&self, t: &Termios) -> bool {
        if t.canonical() {
            // A line terminator needs the whole line to fit.
            self.input.room() > self.line.len()
        } else {
            self.input.room() > 0
        }
    }

    /// Would a read return without waiting (`poll` readability)?
    #[must_use]
    pub fn readable(&self, t: &Termios) -> bool {
        if t.canonical() {
            self.delims > 0
        } else {
            // Linux `n_tty_poll`: with `VTIME` set one byte is enough; without
            // it, `VMIN` bytes (at least one).
            let need = if t.cc[VTIME] != 0 { 1 } else { usize::from(t.cc[VMIN]).max(1) };
            self.input.len() >= need
        }
    }

    /// Run `data` through input processing. Returns bytes consumed (fewer than
    /// offered means the input queue is full).
    pub fn receive_all(&mut self, t: &Termios, data: &[u8], signals: &mut Signals) -> usize {
        let mut accepted = 0;
        for &c in data {
            if !self.receive(t, c, signals) {
                break;
            }
            accepted += 1;
        }
        accepted
    }

    /// Run `data` through output processing. Returns bytes consumed.
    pub fn write_all(&mut self, t: &Termios, data: &[u8]) -> usize {
        let mut n = 0;
        for &c in data {
            if !self.out_char(t, c) {
                break;
            }
            n += 1;
        }
        n
    }

    /// A slave read: `Data` or `Block` (hangup is the pair's business).
    pub fn read_input(&mut self, t: &Termios, out: &mut [u8], expired: bool) -> SlaveRead {
        if out.is_empty() {
            return SlaveRead::Data(0);
        }
        if t.canonical() {
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

        let vmin = usize::from(t.cc[VMIN]);
        let vtime = t.cc[VTIME];
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
    pub fn receive(&mut self, t: &Termios, c: u8, signals: &mut Signals) -> bool {
        if self.lnext {
            self.lnext = false;
            return self.store_literal(t, c);
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
                    self.echo(t, c);
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
                self.echo(t, c);
            }
            return true;
        }

        if t.is(c, VERASE) {
            self.erase_char(t);
            return true;
        }
        if t.is(c, VKILL) {
            self.kill_line(t, c);
            return true;
        }
        if t.lflag & IEXTEN != 0 && t.is(c, VWERASE) {
            self.erase_word(t);
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
                self.echo(t, c);
            }
            return self.finish_line(u16::from(c) | DELIM);
        }
        if self.line.len() < MAX_CANON {
            // Within the capacity reserved in `try_new`: never reallocates.
            self.line.push(c);
            if t.lflag & ECHO != 0 {
                self.echo(t, c);
            }
        }
        // A full line drops the byte (Linux rings the bell); it was still
        // consumed, so the writer is not blocked on a line that can only end.
        true
    }

    /// The byte after `VLNEXT`: data, whatever it is.
    fn store_literal(&mut self, t: &Termios, c: u8) -> bool {
        if t.canonical() {
            if self.line.len() < MAX_CANON {
                self.line.push(c);
            }
        } else if !self.input.push(u16::from(c)) {
            self.lnext = true;
            return false;
        }
        if t.lflag & ECHO != 0 {
            self.echo(t, c);
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
    fn erase_char(&mut self, t: &Termios) {
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
            self.echo(t, t.cc[VERASE]);
        }
    }

    /// `VWERASE`: trailing blanks, then the word before them.
    fn erase_word(&mut self, t: &Termios) {
        while self.line.last().is_some_and(|&b| b == b' ' || b == b'\t') {
            self.erase_char(t);
        }
        while self.line.last().is_some_and(|&b| b != b' ' && b != b'\t') {
            self.erase_char(t);
        }
    }

    /// `VKILL`: the whole line. Linux rubs it out character by character only
    /// with `ECHOE|ECHOK|ECHOKE` all set (the default); otherwise it echoes the
    /// kill character, and a newline if `ECHOK`.
    fn kill_line(&mut self, t: &Termios, kill_char: u8) {
        let lflag = t.lflag;
        if lflag & ECHO == 0 {
            self.line.clear();
            return;
        }
        if lflag & ECHOK == 0 || lflag & ECHOKE == 0 || lflag & ECHOE == 0 {
            self.line.clear();
            self.echo(t, kill_char);
            if lflag & ECHOK != 0 {
                self.out_char(t, b'\n');
            }
            return;
        }
        while !self.line.is_empty() {
            self.erase_char(t);
        }
    }

    /// Echo one input byte: control characters as `^X` under `ECHOCTL`
    /// (except tab and newline), everything through output processing.
    /// Dropped when the output buffer is full, as Linux drops echo.
    fn echo(&mut self, t: &Termios, c: u8) {
        if t.lflag & ECHOCTL != 0 && is_ctl(c) {
            self.out_raw(&[b'^', c ^ 0x40]);
        } else {
            self.out_char(t, c);
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
    pub fn out_char(&mut self, t: &Termios, c: u8) -> bool {
        let o = t.oflag;
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


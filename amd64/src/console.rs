//! The serial console as a `ProcessChannel` — this target's stdio bridge.
//!
//! # Why a channel at all
//!
//! `akuma-syscalls-glue` serves fd 0 from a [`ProcessChannel`]: its
//! `Stdin`/`DevTty` read arm drains `read_stdin`, applies the line discipline
//! held in the process's `TerminalState`, echoes back through `write`, and —
//! when the FIFO is empty — registers an `input_waker` on that state and parks.
//! Every consumer in that arm already exists on this target. **The producer
//! does not.**
//!
//! On the AArch64 kernel the producer is userspace: `sshd` writes
//! `/proc/<pid>/fd/0`, which lands in `akuma_exec::process::write_to_process_stdin`
//! and fills the channel. That kernel never reads a console input device at
//! all — `grep` finds no UART reader in `src/`. This target is the other half of
//! `TTY_SHENANIGANS.md` round 3: here the console **is** an input device, polled
//! destructively through [`crate::input::getb`], and until this module existed
//! it was polled from inside the reading thread itself (`fd::read_console`'s
//! `loop { getb() else yield_now() }`).
//!
//! So this is the missing producer, and it is deliberately shaped as the
//! *console's* answer to `sshd`'s `bridge_process`: one task, both directions.
//!
//! # One channel, one line discipline, every console process
//!
//! [`CHANNEL`] and [`TERM`] are single objects shared by every console-attached
//! process (see `usermode::register_exec_process`), which is not a shortcut —
//! it is what a tty is. Two processes reading the same serial line share one
//! input queue and one set of termios flags; a `fork` child inherits both
//! because it is handed the same `Arc`. The one real limitation is inherited
//! from `TerminalState`: `set_input_waker` holds a single waker, so two
//! concurrently parked console readers means the second displaces the first.
//! That is also true on AArch64 and has never been the interesting case.
//!
//! # The pump owns the hardware
//!
//! Once [`spawn_pump`] runs, **nothing else may call [`crate::input::getb`]** —
//! it is destructive, and a second caller steals bytes out of the console's
//! input queue. `fd::read_console` and `fd::poll_console_state` ask this module
//! instead.

use alloc::sync::Arc;
use spinning_top::Spinlock;

use akuma_exec::process::ProcessChannel;
use akuma_terminal::TerminalState;

/// The console's `ProcessChannel`: keyboard bytes in `stdin_buffer`, the line
/// discipline's echo in `buffer`.
///
/// `None` until [`init`], which `boot::wire_console_and_syscalls` calls at the
/// same point it brought the old `fd::CONSOLE` up — so a read reaching here
/// before the console exists answers exactly as it did then (EOF, not a park).
static CHANNEL: Spinlock<Option<Arc<ProcessChannel>>> = Spinlock::new(None);

/// The console's line discipline, and the `input_waker` [`pump_once`] fires.
///
/// The same `Arc` every console-attached process carries as
/// `Process::terminal_state`, so `current_terminal_state()` inside a folded glue
/// arm and the pump's wake are looking at one object. Handing each process a
/// fresh `TerminalState` would leave the pump waking a cell nobody registered
/// on — a keystroke that arrives and wakes nothing is the failure this whole
/// module exists to avoid.
static TERM: Spinlock<Option<Arc<Spinlock<TerminalState>>>> = Spinlock::new(None);

/// `c_cc[VSUSP]`, which `akuma_terminal::cc_index` does not name.
///
/// The array is 20 bytes and Linux's index for `^Z` is 10; the constant is
/// simply missing from that crate's table, and adding it there would change
/// what the AArch64 kernel's `TCGETS` reports. Spelled here instead, where it
/// costs one line and no other kernel.
const CC_VSUSP: usize = 10;

/// A `TerminalState` carrying **exactly what this target's `TCGETS` used to
/// report from literals**.
///
/// `fd::console_ioctl` answered `TCGETS` from compiled-in constants because
/// there was no line discipline to describe. There is one now, and the ioctl
/// reads it — so the state has to start out saying what the constants said, or
/// the fold would silently re-describe every terminal on this target. Four
/// flag words and one control character differ from
/// `TerminalState::default()`: `c_cflag` (`B38400|CS8|CREAD`, which `stty`
/// prints as the line speed and the default leaves at 0, i.e. hang-up),
/// `c_lflag`'s `ECHOCTL|ECHOKE|IEXTEN`, and `c_cc[VSUSP]`.
///
/// Used for **every** process this target registers, not just the console one:
/// a spawned child's fd 0 is a pipe faked up as a tty by the same `console_ioctl`
/// arm, and it has to keep reading what it read before too.
#[must_use]
pub fn default_terminal_state() -> TerminalState {
    let mut ts = TerminalState::default();
    let (iflag, oflag, cflag, lflag) = DEFAULT_FLAGS;
    ts.iflag = iflag;
    ts.oflag = oflag;
    ts.cflag = cflag;
    ts.lflag = lflag;
    ts.cc[CC_VSUSP] = VSUSP_CHAR;
    ts
}

/// `(c_iflag, c_oflag, c_cflag, c_lflag)` — see [`default_terminal_state`].
///
/// Separate from that function so `fd::console_ioctl` can answer `TCGETS` for a
/// thread with no registered state at all without building a whole
/// `TerminalState` (two heap containers and two spinlocks) to read four words
/// out of it.
pub const DEFAULT_FLAGS: (u32, u32, u32, u32) = (
    0x0000_0500, // c_iflag = ICRNL | IXON
    0x0000_0005, // c_oflag = OPOST | ONLCR
    0x0000_00BF, // c_cflag = B38400 | CS8 | CREAD
    0x0000_8A3B, // c_lflag = ISIG|ICANON|ECHO|ECHOE|ECHOK|ECHOCTL|ECHOKE|IEXTEN
);

/// `^Z`, the one control character [`default_terminal_state`] adds to
/// `TerminalState`'s own defaults.
pub const VSUSP_CHAR: u8 = 0x1A;

/// The `c_cc` array a thread with no registered terminal state reports.
///
/// `TerminalState::default()`'s control characters, plus [`VSUSP_CHAR`], as a
/// plain array — the allocation-free half of [`default_terminal_state`], for the
/// same caller and the same reason as [`DEFAULT_FLAGS`].
#[must_use]
pub fn default_cc() -> [u8; 20] {
    let mut cc = [0u8; 20];
    cc[akuma_terminal::cc_index::VINTR] = 0x03;
    cc[akuma_terminal::cc_index::VQUIT] = 0x1C;
    cc[akuma_terminal::cc_index::VERASE] = 0x7F;
    cc[akuma_terminal::cc_index::VKILL] = 0x15;
    cc[akuma_terminal::cc_index::VEOF] = 0x04;
    cc[akuma_terminal::cc_index::VTIME] = 0x00;
    cc[akuma_terminal::cc_index::VMIN] = 0x01;
    cc[CC_VSUSP] = VSUSP_CHAR;
    cc
}

/// Bring the console channel and its line discipline up. Called once, from
/// `boot::wire_console_and_syscalls`, before ring 3 exists.
pub fn init() {
    let ch = Arc::new(ProcessChannel::new());
    // `ProcessChannel::new` already defaults this to `true`; stated rather than
    // assumed, because it is the flag glue's `openat` gates `/dev/tty` on and
    // the one that selects cooked input over raw pipe pass-through.
    ch.set_terminal(true);
    *CHANNEL.lock() = Some(ch);
    *TERM.lock() = Some(Arc::new(Spinlock::new(default_terminal_state())));
}

/// The console channel, or `None` before [`init`].
#[must_use]
pub fn channel() -> Option<Arc<ProcessChannel>> {
    CHANNEL.lock().clone()
}

/// The console's shared line discipline, or `None` before [`init`].
#[must_use]
pub fn terminal_state() -> Option<Arc<Spinlock<TerminalState>>> {
    TERM.lock().clone()
}

/// Is there input waiting in the console's queue?
///
/// **Not [`crate::input::has_byte`]** — that asks the hardware, and once the
/// pump runs the hardware is drained into the channel within a lap. Asking the
/// UART would report "nothing" while a typed line sits in the FIFO, and report
/// "something" for a byte the reader cannot yet see.
#[must_use]
pub fn has_input() -> bool {
    CHANNEL.lock().as_ref().is_some_and(|ch| ch.has_stdin_data())
}

/// How many bytes of one lap's keyboard input the pump moves at a time.
///
/// A stack array, not a `Vec`: this runs on every lap of a daemon for the life
/// of the kernel, and a per-lap heap allocation on the path that keeps an
/// interactive console alive is the kind the allocator rules exist to refuse.
const PUMP_IN: usize = 64;

/// The same, for the echo the line discipline writes back.
const PUMP_OUT: usize = 256;

/// One lap of the bridge. Returns whether anything moved in either direction.
///
/// Both directions, deliberately: the line discipline's echo is written to the
/// channel's *stdout* FIFO (`ProcessChannel::write`, which is where glue's
/// `Stdin` arm puts `ProcessedInput::echo`), and on an ssh session `sshd` is
/// what drains that back to the client. On the serial console there is no
/// `sshd`, so the pump is also the drain — without it a typed character would
/// never appear and the FIFO would climb to its 1 MiB cap.
pub fn pump_once() -> bool {
    let Some(ch) = channel() else {
        return false;
    };
    let mut moved = false;

    // The machine's input devices -> the console's input queue.
    let mut inbuf = [0u8; PUMP_IN];
    let mut n = 0;
    while n < inbuf.len() {
        match crate::input::getb() {
            Some(b) => {
                inbuf[n] = b;
                n += 1;
            }
            None => break,
        }
    }
    if n > 0 {
        // A short write means the queue is at its cap, i.e. nobody is reading;
        // the excess is dropped, which is what a tty input queue does when it
        // overflows. Not retried — retrying here would spin against a reader
        // that is not there.
        ch.write_stdin(&inbuf[..n]);
        wake_reader();
        moved = true;
    }

    // The line discipline's echo -> the serial port and the framebuffer.
    //
    // `has_stdout_data` first, and that guard is load-bearing: an *empty*
    // `ProcessChannel::read` calls `add_poller(current_thread_id())`, so
    // reading unconditionally would register this daemon as a poller on the
    // console channel on every idle lap.
    let mut outbuf = [0u8; PUMP_OUT];
    while ch.has_stdout_data() {
        let n = ch.read(&mut outbuf);
        if n == 0 {
            break;
        }
        for &b in &outbuf[..n] {
            crate::serial::putb(b);
        }
        moved = true;
    }

    moved
}

/// Release a reader parked in glue's `Stdin` arm.
///
/// Verbatim the shape `akuma_exec::process::write_to_process_stdin` uses on the
/// other kernel — `lock_bounded` so preemption is disabled for a single
/// `try_lock` attempt rather than across the wait, which is what
/// `docs/archive/TERM_POLL_INPUT_PREEMPTION_FIX.md` §10 is about — because the
/// waiter it has to release is the same waiter.
fn wake_reader() {
    let Some(term) = terminal_state() else {
        return;
    };
    let waker = akuma_exec::sync::lock_bounded(&term).input_waker.lock().take();
    if let Some(waker) = waker {
        waker.wake();
    }
}

/// The pump, as a daemon task.
///
/// # Why it yields rather than parking on a deadline
///
/// `sched::block_until_deadline` resolves at the LAPIC tick, and the timer is
/// not running on every boot this target supports (`main.rs` starts it only
/// when there is a network). A pump that parks on a clock that is not ticking
/// is a console that never delivers a keystroke — the one failure this module
/// must not have. `yield_now` needs nothing but the round-robin, and it is the
/// same cost profile this target already accepts: `net::netpoll_daemon` is a
/// permanent yield loop, and `fd::read_console` was one for the whole time a
/// shell sat at a prompt. The difference is that this one also runs while
/// nothing is reading, which is the honest price of a polled console with no
/// IOAPIC behind it.
extern "C" fn pump_daemon() -> ! {
    loop {
        pump_once();
        crate::sched::yield_now();
    }
}

/// Whether [`spawn_pump`] has already run.
static PUMP_SPAWNED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Start the pump. Idempotent — the second caller is a no-op rather than a
/// second daemon racing the first for the same destructive `getb`.
///
/// Returns whether a pump is running when this returns.
pub fn spawn_pump() -> bool {
    if PUMP_SPAWNED.swap(true, core::sync::atomic::Ordering::AcqRel) {
        return true;
    }
    if crate::sched::spawn_daemon(pump_daemon).is_some() {
        return true;
    }
    // The task table was full. Hand the flag back so a later caller can try
    // again rather than believing a pump it never got.
    PUMP_SPAWNED.store(false, core::sync::atomic::Ordering::Release);
    false
}

/// Is the pump running? Read by `fd::read_console` to tell "the console is idle"
/// from "nothing is filling the console", which are the same empty queue and
/// very different answers.
#[must_use]
pub fn pump_running() -> bool {
    PUMP_SPAWNED.load(core::sync::atomic::Ordering::Acquire)
}

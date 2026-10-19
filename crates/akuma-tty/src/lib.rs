//! The terminal stack, as pure state machines.
//!
//! Everything in this crate decides; nothing in it locks, allocates on a path
//! that runs under memory pressure without saying so, signals, wakes or touches
//! user memory. The kernel keeps the acting half (`akuma-syscalls-glue`'s
//! `term.rs`/`pty.rs`, `akuma-exec`'s channel) and calls in.
//!
//! | module | what | consumers |
//! |---|---|---|
//! | [`termios`] | flag bits, `c_cc` indices, ioctl numbers, [`Termios`]/[`Winsize`] wire layouts, [`Transition`] | everyone |
//! | [`ld`] | [`ld::LineDiscipline`]: the engine — buffers, editing, echo, `ISIG`, read decisions |
//! | [`session`] | [`TerminalState`]: the console / ssh-session terminal, on the engine | glue `read(stdin)`/`TCSETS`, `akuma-exec`, amd64 console |
//! | [`pty`] | [`pty::PtyPair`]: Unix98 pairs, on the engine | glue `pty.rs` |
//!
//! # Who owns the raw/canonical bit
//!
//! `ICANON` in `c_lflag` is the **only** copy of "is this terminal raw?":
//!
//! - session terminals: [`TerminalState::lflag`], read through
//!   [`TerminalState::is_canonical`], written only by
//!   [`TerminalState::set_termios`] (`TCSETS*`) and
//!   [`TerminalState::set_raw_mode`] (`SET_TERMINAL_ATTRIBUTES`);
//! - pty slaves: `PtyPair`'s private `termios`, written only by
//!   [`pty::PtyPair::set_termios`].
//!
//! Each returns a [`Transition`]; `Transition::Cooked` is the raw -> canonical
//! edge that left a TUI's keys stuck in the line buffer
//! (`docs/archive/AMD64_CLAUDE_LOGIN_INPUT_STUCK.md`), so
//! `grep -rn 'Transition::Cooked'` lists every place that may react to it.
//! There used to be a channel-side `raw_mode` flag and a copy in the amd64
//! console; both are gone.
//!
//! # One discipline
//!
//! [`ld::LineDiscipline`] is the only implementation of the N_TTY rules (input
//! processing, canonical editing, echo, `ISIG`, `VMIN`/`VTIME`, output
//! processing). `pty::PtyPair` wraps it with a master/slave pair's refcounts,
//! hangup rules and waiters; `session::TerminalState` wraps it (allocated on the
//! first canonical read) with a session's window size, foreground group and the
//! raw-mode request. `differential.rs` feeds both the same keystrokes and
//! asserts identical echo and reads. What still differs is *when* it runs — a
//! pty applies it on write, a session on `read` — and `translate_output`
//! (stateless, ignores `OPOST`); see `docs/reference/subsystems/tty.md`.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

mod ring;
pub mod ld;
pub mod pty;
pub mod session;
pub mod termios;

#[cfg(test)]
mod differential;

pub use ld::LineDiscipline;
pub use ring::Ring;
pub use session::{CanonRead, Ingest, NoMemory, TerminalState};
pub use termios::{Termios, Transition, Winsize};

/// Custom Akuma `SET_TERMINAL_ATTRIBUTES` request bits plus the old
/// `akuma-terminal` flag names, now aliases of [`termios::flags`].
pub mod mode_flags {
    pub use crate::termios::flags::{ECHO, ECHOE, ECHOK, ECHONL, ICANON, ICRNL, ISIG, IXON, ONLCR, OPOST};
    /// Enable raw mode (disable canonical, echo, ISIG)
    pub const RAW_MODE_ENABLE: u64 = 0x01;
    /// Disable raw mode (restore canonical, echo, ISIG)
    pub const RAW_MODE_DISABLE: u64 = 0x02;
}

/// `c_cc` indices (the old `akuma-terminal` spelling of [`termios::cc`]).
pub use termios::cc as cc_index;

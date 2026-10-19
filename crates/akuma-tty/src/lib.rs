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
//! | [`session`] | [`TerminalState`]: the console / ssh-session line discipline | glue `read(stdin)`/`TCSETS`, `akuma-exec`, amd64 console |
//! | [`pty`] | [`pty::PtyPair`]: Unix98 pairs with a full N_TTY | glue `pty.rs` |
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
//! # Two disciplines, one vocabulary (open)
//!
//! `session` and `pty` are still two line-discipline engines. They differ in
//! behaviour a caller can observe (`PtyPair` echoes `^X` under `ECHOCTL`, has
//! word-erase and literal-next, raises signals from the input path; `session`
//! does none of that and leaves ISIG to the channel write). Merging them is a
//! behaviour change and is tracked in `docs/reference/subsystems/tty.md`.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

mod ring;
pub mod pty;
pub mod session;
pub mod termios;

pub use ring::Ring;
pub use session::{ProcessedInput, TerminalState};
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

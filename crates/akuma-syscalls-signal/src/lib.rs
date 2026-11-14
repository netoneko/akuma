//! The **signal bookkeeping** both x86_64 kernels need, as pure logic.
//!
//! Taoxichuan delivers no signals: `rt_sigaction` only has to *remember* what
//! the task installed and read it back (musl's `signal()` returns the OLD
//! handler from `oldact`; std's child-side `signal(SIGPIPE, SIG_DFL)` fails the
//! whole spawn when that reads back as `SIG_ERR`), and `rt_sigprocmask` only has
//! to keep a word. Both of those are in here, host-tested, so the amd64 kernel's
//! delivery code (`crates/akuma-kernel-amd64/src/signal.rs`) can adopt the same decode arms.
//!
//! What stays with the kernel: the user-memory copies (`act`, `oldact`, `set`,
//! `oldset`), the errno table, per-task storage, and anything that *acts* on a
//! signal.
//!
//! # Order of checks (pinned — Linux's, and the previous in-kernel code's)
//! `rt_sigaction`: `sigsetsize != 8`, then `sig` out of `1..=64`, then an `act`
//! for `SIGKILL`/`SIGSTOP` — all `EINVAL`, in that order, *before* any user
//! memory is touched. `rt_sigprocmask`: `sigsetsize != 8`, then (only if a `set`
//! was given) `how > 2` — `EINVAL`.

#![no_std]
#![forbid(unsafe_code)]

/// `SIGKILL`: can be neither caught, ignored nor blocked.
pub const SIGKILL: u64 = 9;
/// `SIGSTOP`: likewise.
pub const SIGSTOP: u64 = 19;
/// Highest signal number (the kernel `sigset_t` is one 64-bit word).
pub const NSIG: u64 = 64;
/// `sizeof(kernel sigset_t)`.
pub const SIGSET_SIZE: u64 = 8;
/// `SIG_DFL` / `SIG_IGN` as a handler word.
pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;
/// Bytes of the kernel `struct sigaction`: handler, flags, restorer, mask.
pub const SIGACTION_WIRE: usize = 32;

/// The only error these decisions produce; the caller maps it to `-EINVAL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SigError {
    Inval,
}

/// `stack_t.ss_flags` disable bit.
pub const SS_DISABLE: i32 = 2;
/// `MINSIGSTKSZ`: an enabled alternate stack smaller than this is `ENOMEM`.
pub const MINSIGSTKSZ: u64 = 2048;

/// What `sigaltstack(ss, ..)` was asked to do, once the flags are checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AltStackChange {
    /// `SS_DISABLE`: the task goes back to having no alternate stack.
    Disable,
    /// Install the caller's `{sp, flags, size}` as given.
    Install,
}

/// The two ways `sigaltstack` refuses a new stack (the caller maps them to errno).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AltStackError {
    /// `EINVAL`: a flag the caller may not set.
    Inval,
    /// `ENOMEM`: enabled, but smaller than `MINSIGSTKSZ`.
    NoMem,
}

/// `sigaltstack`'s decision on a new `stack_t`. `SS_ONSTACK` (1) is the
/// kernel's to report and the caller's to never set; only `SS_AUTODISARM`
/// (`1 << 31`) may ride along with the mode. `SS_DISABLE` wins over `size`.
pub fn altstack_change(flags: i32, size: u64) -> Result<AltStackChange, AltStackError> {
    if flags & !(SS_DISABLE | i32::MIN) != 0 {
        return Err(AltStackError::Inval);
    }
    if flags & SS_DISABLE != 0 {
        Ok(AltStackChange::Disable)
    } else if size < MINSIGSTKSZ {
        Err(AltStackError::NoMem)
    } else {
        Ok(AltStackChange::Install)
    }
}

/// One kernel `struct sigaction`: `[handler, flags, restorer, mask]`.
pub type SigAction = [u64; 4];

/// Bit of signal `sig` (1-based) in a `sigset_t` word.
#[must_use]
pub const fn bit(sig: u64) -> u64 {
    1 << (sig - 1)
}

/// Signals no task may block.
pub const UNBLOCKABLE: u64 = bit(SIGKILL) | bit(SIGSTOP);

/// The wire image of a `struct sigaction` (little-endian words).
#[must_use]
pub fn to_wire(a: &SigAction) -> [u8; SIGACTION_WIRE] {
    let mut out = [0u8; SIGACTION_WIRE];
    for (i, w) in a.iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    out
}

/// Parse a `struct sigaction` wire image.
#[must_use]
pub fn from_wire(b: &[u8; SIGACTION_WIRE]) -> SigAction {
    let mut a = [0u64; 4];
    for (i, w) in a.iter_mut().enumerate() {
        let mut word = [0u8; 8];
        word.copy_from_slice(&b[i * 8..i * 8 + 8]);
        *w = u64::from_le_bytes(word);
    }
    a
}

/// A task's `rt_sigaction` table, signals `1..=64` (slot 0 unused). `Copy`: a
/// fork child inherits it by value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SigTable {
    act: [SigAction; 65],
}

impl Default for SigTable {
    fn default() -> Self {
        Self::new()
    }
}

impl SigTable {
    #[must_use]
    pub const fn new() -> Self {
        Self { act: [[0; 4]; 65] }
    }

    /// Validate an `rt_sigaction` call *before the caller touches user memory*.
    ///
    /// # Errors
    /// `Inval` for a wrong `sigsetsize`, a signal outside `1..=64`, or an
    /// attempt (`has_act`) to change `SIGKILL`/`SIGSTOP`.
    pub fn check(sig: u64, has_act: bool, sigsetsize: u64) -> Result<(), SigError> {
        if sigsetsize != SIGSET_SIZE || sig == 0 || sig > NSIG {
            return Err(SigError::Inval);
        }
        if has_act && (sig == SIGKILL || sig == SIGSTOP) {
            return Err(SigError::Inval);
        }
        Ok(())
    }

    /// The installed action for `sig` (`1..=64`; anything else reads as the
    /// default action, never panics).
    #[must_use]
    pub fn get(&self, sig: u64) -> SigAction {
        self.act.get(sig as usize).copied().unwrap_or([0; 4])
    }

    /// Install `new` for `sig` and return the old action. Out-of-range or
    /// slot-0 signals are ignored (and read back as default) — `check` is the
    /// gate.
    pub fn replace(&mut self, sig: u64, new: SigAction) -> SigAction {
        match self.act.get_mut(sig as usize) {
            Some(slot) if sig != 0 => core::mem::replace(slot, new),
            _ => [0; 4],
        }
    }

    /// `execve`: caught handlers go back to `SIG_DFL`; ignored ones stay ignored
    /// (POSIX: an ignored signal stays ignored across exec, a caught one cannot,
    /// its handler's code is gone).
    pub fn reset_for_exec(&mut self) {
        for a in &mut self.act {
            if a[0] != SIG_IGN {
                *a = [0; 4];
            }
        }
    }
}

/// `rt_sigprocmask`: how the mask changes. `how`: 0 `SIG_BLOCK`, 1
/// `SIG_UNBLOCK`, 2 `SIG_SETMASK`.
///
/// # Errors
/// `Inval` for `sigsetsize != 8`, or (only when a `set` was supplied, `new` is
/// `Some`) a `how` above 2.
pub fn procmask(how: u64, cur: u64, new: Option<u64>, sigsetsize: u64) -> Result<u64, SigError> {
    if sigsetsize != SIGSET_SIZE {
        return Err(SigError::Inval);
    }
    let Some(set) = new else {
        return Ok(cur);
    };
    if how > 2 {
        return Err(SigError::Inval);
    }
    let bits = set & !UNBLOCKABLE;
    Ok(match how {
        0 => cur | bits,
        // UNBLOCK clears whatever it is asked to, SIGKILL/SIGSTOP included: they are never *set* by this
        // function, and a stray bit (a mask installed some other way) must still be removable.
        1 => cur & !set,
        _ => bits,
    })
}

// ── tkill's decision ─────────────────────────────────────────────────────────

/// Does `sig` (1..=64) terminate the process when no handler is installed?
///
/// Linux's table: every signal except those whose default is to ignore
/// (`SIGCHLD` 17, `SIGURG` 23, `SIGWINCH` 28) or to stop or continue (18..=22;
/// no job control here, so those are no-ops rather than kills). That includes
/// `SIGUSR1`/`SIGUSR2` and the real-time signals 32..=64.
#[must_use]
pub const fn fatal_by_default(sig: u64) -> bool {
    matches!(sig, 1..=64) && !matches!(sig, 17..=23 | 28)
}

/// The target thread's process-wide disposition for the signal (`sigaction`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    Ignore,
    Default,
    /// A user handler.
    Handler,
}

/// What `tkill`/`tgkill` does with a signal (already validated as 1..=64).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TkillAction {
    /// Nothing happens; the call still succeeds.
    Drop,
    /// Set the pending bit on the target thread; it runs when unmasked, at the
    /// target's next return to user mode.
    Pend,
    /// Pend, and interrupt the target out of any park so it acts promptly (a
    /// fatal default it cannot block). The target dies at its own next return to
    /// user mode, through its kernel's delivery path.
    PendInterrupt,
}

/// `tkill`'s decision, for both kernels.
///
/// `target` is `None` when the thread has no owning process (a kernel thread,
/// or a process already gone), else the target's disposition for `sig`. `mask`
/// is the *target thread's* blocked set.
///
/// The rules (and why):
/// - **No owning process: drop.** There is no disposition to run. Reading that
///   as `Default` and acting on it is what made a `SIGUSR1` storm kill `rustc`
///   (`docs/archive/SIGNAL_FATAL_DEFAULT_LINUX.md`).
/// - `SIGKILL` cannot be blocked, ignored or handled: pend and interrupt.
/// - **Blocked: pend, whatever the disposition.** Linux never discards a blocked
///   signal ("the handler may change by the time it is unblocked"), so a program
///   that blocks `SIGCHLD` and `sigtimedwait`s for it sees it. And never
///   interrupt for one: a blocked signal that ended a wait with `EINTR` made
///   musl's retry loops spin.
/// - A handler pends (delivered at the next return to user mode).
/// - `Default`, unblocked: a fatal one pends and interrupts; one whose default is
///   to ignore (or stop/continue, no-ops here) is dropped. `Ignore`: dropped.
///
/// **No fatal default acts inside `tkill`** (2026-11-11). Until then a fatal
/// default aimed at the caller's own group was `ExitOwnGroup`: `sys_exit_group`
/// called from inside the `tkill`. That is the AArch64 exit and the wrong one on
/// amd64, which left ring 3 through glue's epilogue instead of `run_process`'s;
/// pending it lets each kernel's delivery run the exit the way that kernel leaves
/// user mode, and for `tkill(gettid(), …)` that is the return this call is about
/// to make (`docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`, "Slice 3: signals").
#[must_use]
pub const fn tkill_action(sig: u64, target: Option<Disposition>, mask: u64) -> TkillAction {
    let Some(disposition) = target else {
        return TkillAction::Drop;
    };
    if sig == SIGKILL {
        return TkillAction::PendInterrupt;
    }
    if mask & bit(sig) != 0 {
        return TkillAction::Pend;
    }
    match disposition {
        Disposition::Ignore => TkillAction::Drop,
        Disposition::Handler => TkillAction::Pend,
        Disposition::Default => {
            if fatal_by_default(sig) {
                TkillAction::PendInterrupt
            } else {
                TkillAction::Drop
            }
        }
    }
}

#[cfg(test)]
mod tests;

//! The `/proc/<pid>` virtual-file byte formats `ps` and `top` parse.
//!
//! This is the wire layer and nothing else: every function here takes a
//! [`ProcStat`] — five scalars and a string — and writes bytes into a caller's
//! buffer. There is no process table, no lock, no filesystem, and deliberately
//! no dependency that could name a `Process`.
//!
//! # Why this is a crate
//!
//! Two kernels serve `/proc` and neither can share the other's code. The
//! AArch64 side has a full `ProcFilesystem` (`akuma-vfs-glue`) behind the VFS
//! mount table; amd64 has no mount table at all and answers a handful of
//! `/proc` paths straight out of `fd::sys_openat`. `akuma-vfs-glue` cannot even
//! be *compiled* for `x86_64-unknown-none` — it reaches `akuma-exec` and
//! `akuma-elf`, which are written against `akuma-mmu`. So the filesystem cannot
//! move; the formats can, and the formats are the part where a divergence is
//! both easy and invisible.
//!
//! This is the same seam, and the same reason, as `akuma-syscalls-net`: a
//! read-only introspection surface whose *byte layout* is what busybox reads,
//! shared so the two kernels cannot drift on a field offset.
//!
//! # The 44-field line is the whole point
//!
//! `/proc/<pid>/stat` is a single space-separated line and `ps` finds `utime`
//! by counting to field 14. A field added, dropped or misordered does not
//! produce an error anywhere — it produces a `ps` that prints a plausible wrong
//! number, or an empty process list. The AArch64 side already paid for that
//! once: `status` and `cmdline` were both correct and complete, `ps` showed
//! nothing at all, and the reason was that `stat` — the file `ps` actually
//! parses — did not exist. That is a pure-function bug, and
//! [`render_pid_stat`]'s tests cost a millisecond.
//!
//! # What callers still own
//!
//! Whose processes are visible, what a pid means, where `comm` comes from, and
//! every lock taken to read them. A caller maps its own process type onto
//! [`ProcStat`] and passes it in.

#![no_std]
#![forbid(unsafe_code)]

use akuma_primitives::console::FmtBuf;
use core::fmt::Write as _;

/// USER_HZ: the jiffy rate every `/proc` CPU-time field is expressed in.
///
/// 100 Hz on every architecture that matters here, **regardless of the kernel's
/// actual timer tick**. It is an ABI constant — what `sysconf(_SC_CLK_TCK)`
/// reports — not a reflection of how often the kernel actually interrupts, so
/// deriving it from a timer-interval config would be wrong in a way that only
/// shows up as `ps` reporting impossible CPU times.
pub const JIFFY_US: u64 = 10_000;

/// The capability mask a full-root process reports: every capability Linux
/// defines up to `CAP_LAST_CAP`.
///
/// Neither kernel has a capability model — everything runs as root — but the
/// `Cap*` lines still have to be present, because that is where **libcap-ng**
/// reads a process's capabilities from: `capng_get_caps_process` parses
/// `/proc/self/status`, it does not call `capget`. With the lines absent it
/// fails, and `capng_apply` then returns -1 *without setting errno* — which is
/// exactly the `setpriv: activate capabilities: No error information` that
/// killed `redis:alpine`'s entrypoint. Stubbing `capset(2)` did not help,
/// because `capset` was never the call that failed.
pub const CAP_FULL_MASK: &str = "000001ffffffffff";

/// Linux truncates `comm` to 15 bytes plus a NUL (`TASK_COMM_LEN - 1`).
pub const COMM_LEN: usize = 15;

/// A process's run state, as `/proc` spells it.
///
/// Three states rather than Linux's full set because that is what both kernels
/// actually distinguish. A kernel that grows a real stopped/traced state adds
/// an arm here and both `/proc` implementations report it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProcState {
    /// Runnable or on a CPU. `R`.
    Running,
    /// Blocked in an interruptible wait. `S`.
    Sleeping,
    /// Exited, not yet reaped, carrying its exit code. `Z`.
    Zombie(i32),
}

impl ProcState {
    /// The single character `/proc/<pid>/stat` field 3 uses.
    #[must_use]
    pub const fn stat_char(self) -> char {
        match self {
            Self::Running => 'R',
            Self::Sleeping => 'S',
            Self::Zombie(_) => 'Z',
        }
    }

    /// The `State:` line `/proc/<pid>/status` uses — the character *and* the
    /// parenthesised word, which is the form libraries grep for.
    #[must_use]
    pub const fn status_text(self) -> &'static str {
        match self {
            Self::Running => "R (running)",
            Self::Sleeping => "S (sleeping)",
            Self::Zombie(_) => "Z (zombie)",
        }
    }

    /// The exit code, for a zombie.
    #[must_use]
    pub const fn exit_code(self) -> Option<i32> {
        match self {
            Self::Zombie(code) => Some(code),
            _ => None,
        }
    }
}

/// Everything the `/proc` renderers need about one process.
///
/// Five scalars and a borrowed name. A caller builds one from whatever its own
/// process table holds; this crate never sees the table.
#[derive(Clone, Copy, Debug)]
pub struct ProcStat<'a> {
    pub pid: u32,
    pub ppid: u32,
    pub state: ProcState,
    /// The program's name or path. [`comm`] takes the basename and truncates;
    /// callers pass whatever they have and do not pre-trim.
    pub name: &'a str,
    /// Total CPU time in microseconds. Rendered as jiffies in `utime`; this
    /// kernel does not split user from system time, so `stime` reads 0.
    pub cpu_time_us: u64,
}

impl ProcStat<'_> {
    /// Linux's `comm`: the basename of [`Self::name`], truncated to
    /// [`COMM_LEN`] bytes.
    ///
    /// Truncation is by **bytes on a char boundary**, not by chars: `comm` is a
    /// fixed-width field in the kernel Linux copies this from, and slicing a
    /// `&str` mid-UTF-8 would panic. A name whose 15-byte cut lands inside a
    /// multi-byte character is trimmed to the boundary below it.
    #[must_use]
    pub fn comm(&self) -> &str {
        let base = self.name.rsplit('/').next().unwrap_or(self.name);
        if base.len() <= COMM_LEN {
            return base;
        }
        let mut end = COMM_LEN;
        while end > 0 && !base.is_char_boundary(end) {
            end -= 1;
        }
        &base[..end]
    }

    /// [`Self::cpu_time_us`] in `USER_HZ` jiffies, as `/proc` reports time.
    #[must_use]
    pub const fn utime_jiffies(&self) -> u64 {
        self.cpu_time_us / JIFFY_US
    }
}

/// `/proc/<pid>/stat` — one space-separated line, newline-terminated.
///
/// Returns the bytes written, which is 0 if `buf` was too small to hold any of
/// it and a short count if it filled up mid-line ([`FmtBuf`] truncates rather
/// than failing). Callers wanting the whole line unconditionally should pass at
/// least [`STAT_LINE_MAX`].
///
/// **This is the file `ps` and `top` parse** — not `status`. The fields, in
/// order, are Linux's: `pid comm state ppid pgrp session tty_nr tpgid flags
/// minflt cminflt majflt cmajflt utime stime cutime cstime priority nice
/// num_threads itrealvalue starttime vsize rss rsslim startcode endcode
/// startstack kstkesp kstkeip signal blocked sigignore sigcatch wchan nswap
/// cnswap exit_signal processor rt_priority policy delayacct_blkio_ticks
/// guest_time cguest_time`.
///
/// Everything neither kernel tracks per-process — memory, scheduling, fault
/// counts — reads as 0 or a neutral placeholder. That is enough for `ps`/`top`
/// to list PID/PPID/STATE/COMMAND and a CPU time; real per-process memory
/// accounting is a separate, larger feature and a zero is an honest answer
/// rather than a fabricated one.
///
/// `pgrp` and `session` report the pid itself: neither kernel has process
/// groups, so every process is its own leader, which is the same answer their
/// `getpgrp`/`getsid` syscalls give.
pub fn render_pid_stat(p: &ProcStat, buf: &mut [u8]) -> usize {
    let (pid, ppid) = (p.pid, p.ppid);
    let comm = p.comm();
    let state = p.state.stat_char();
    let utime = p.utime_jiffies();
    let mut pos = 0usize;
    let mut w = FmtBuf { buf, pos: &mut pos };
    let _ = writeln!(
        w,
        "{pid} ({comm}) {state} {ppid} {pid} {pid} 0 -1 0 0 0 0 0 {utime} 0 0 0 20 0 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0"
    );
    pos
}

/// A buffer size [`render_pid_stat`] can never overflow.
///
/// The fixed text is under 100 bytes; the variable parts are three `u32`/`u64`
/// decimals and a [`COMM_LEN`]-byte `comm`.
pub const STAT_LINE_MAX: usize = 256;

/// `/proc/<pid>/status` — the human-readable form.
///
/// Returns bytes written. `Uid`/`Gid` are all-zero (both kernels run everything
/// as root) and the `Vm*` figures are 0 for the same reason `stat`'s memory
/// fields are.
///
/// One format string for every state. The four arms this replaced on the
/// AArch64 side were identical apart from the state word and the zombie's
/// trailing `ExitCode`, which meant adding a field meant remembering to add it
/// four times.
pub fn render_status(p: &ProcStat, buf: &mut [u8]) -> usize {
    let name = p.comm();
    let state = p.state.status_text();
    let (pid, ppid) = (p.pid, p.ppid);
    let mut pos = 0usize;
    let mut w = FmtBuf { buf, pos: &mut pos };
    let _ = write!(
        w,
        "Name:\t{name}\nState:\t{state}\nTgid:\t{pid}\nPid:\t{pid}\nPPid:\t{ppid}\nTracerPid:\t0\n\
         Uid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\nFDSize:\t256\nGroups:\t\n\
         VmPeak:\t0 kB\nVmSize:\t0 kB\nVmRSS:\t0 kB\nThreads:\t1\n\
         CapInh:\t0000000000000000\nCapPrm:\t{CAP_FULL_MASK}\nCapEff:\t{CAP_FULL_MASK}\n\
         CapBnd:\t{CAP_FULL_MASK}\nCapAmb:\t0000000000000000\nNoNewPrivs:\t0\nSeccomp:\t0\n"
    );
    if let Some(code) = p.state.exit_code() {
        let _ = writeln!(w, "ExitCode:\t{code}");
    }
    pos
}

/// A buffer size [`render_status`] can never overflow.
pub const STATUS_MAX: usize = 1024;

/// `/proc/<pid>/cmdline` — argv, each element NUL-terminated.
///
/// Returns bytes written. `args` is the argument vector; when it is empty the
/// process name is emitted as a single element instead, because a `cmdline`
/// that is genuinely empty is how Linux marks a *kernel* thread and `ps` then
/// renders the name in brackets — which is not what an ordinary process with an
/// argv the kernel simply failed to keep should look like.
///
/// Note the trailing NUL after the **last** element: Linux terminates every
/// element including the final one, and `ps` splits on NUL, so omitting it
/// merges the last argument with whatever the reader had in its buffer.
pub fn render_cmdline<'a>(
    p: &ProcStat,
    args: impl IntoIterator<Item = &'a [u8]>,
    buf: &mut [u8],
) -> usize {
    let mut pos = 0usize;
    let mut push = |bytes: &[u8], pos: &mut usize| {
        let n = bytes.len().min(buf.len().saturating_sub(*pos));
        buf[*pos..*pos + n].copy_from_slice(&bytes[..n]);
        *pos += n;
    };
    let mut any = false;
    for arg in args {
        any = true;
        push(arg, &mut pos);
        push(&[0], &mut pos);
    }
    if !any {
        push(p.name.as_bytes(), &mut pos);
        push(&[0], &mut pos);
    }
    pos
}

#[cfg(test)]
mod tests;

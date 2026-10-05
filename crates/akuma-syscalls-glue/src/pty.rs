//! Pseudo-terminals: `/dev/ptmx`, `/dev/pts/N`, and `/dev/tty` inside a pty
//! session — the kernel half of [`akuma_pty`].
//!
//! The pair itself (both buffers, the line discipline, termios, the window
//! size, the read decisions, readiness and hangup) is the crate's, host-tested.
//! This module is what the crate deliberately does not do:
//!
//! - **The table and its lock.** [`MAX_PTYS`] slots, each an `Option` of a pair
//!   plus a generation; the pair's buffers are the only allocation, made once
//!   at `open("/dev/ptmx")` and fallibly (`ENOMEM`, never an abort).
//! - **Descriptors**: `FileDescriptor::PtyMaster(N)`/`PtySlave(N)`, their
//!   reference counts (`pty_clone_ref`/`pty_close`, which `akuma-exec` reaches
//!   through its runtime hooks for `fork` and exit), `O_NONBLOCK`.
//! - **Waiting**: a blocked reader or writer registers its `WakeHandle` on the
//!   pair *under the lock that found it must wait* (the pipes' TOCTOU rule), and
//!   every state change wakes all registered waiters after the lock is dropped.
//!   `VTIME` is a deadline on that park.
//! - **Signals and sessions**: `^C`/`^\`/`^Z` reach the foreground process
//!   group, a resize sends `SIGWINCH`, the last master close sends `SIGHUP` to
//!   the session leader and the foreground group. A process's controlling
//!   terminal is `Process::ctty`, an encoded `(slot, generation)` id, so a value
//!   that outlives its pair names nothing rather than the slot's next tenant.
//!
//! Every signal is delivered **after** the table lock is released: a default
//! disposition terminates inline, which closes descriptors, which comes back
//! here for the lock — the pipe module's 2026-07-24 deadlock, by construction
//! impossible to repeat.
//!
//! Spec, call by call: `docs/archive/AKUMA_AMD64_RIO_FBDEV_BUILD.md`,
//! "Kernel spec: Linux ptys". Why it exists: rio, and every other terminal
//! emulator, needs `openpty` to work (`docs/archive/AKUMA_AMD64_PTY.md`).

use super::*;
use akuma_exec::threading::{WakeHandle, wake_handle_for_thread};
use akuma_pty::{
    HungUp, MAX_PTYS, MasterRead, PtyPair, SlaveOpenError, SlaveRead, Termios, Winsize, ioctl as req, sig,
};
use akuma_syscalls_linux::flags::open::{O_CLOEXEC, O_NONBLOCK};
use akuma_syscalls_poll::readiness::FdState;

struct Slot {
    pair: Option<PtyPair<WakeHandle>>,
    /// Bumped every time the slot is handed out, so a stale controlling-tty id
    /// stops matching. 24 bits survive the encoding.
    generation: u32,
}

impl Slot {
    const EMPTY: Self = Self { pair: None, generation: 0 };
}

/// The pairs. ~400 bytes a slot of `.bss`; a pair's buffers live on the heap
/// only while it is open.
static PTYS: Spinlock<[Slot; MAX_PTYS]> = Spinlock::new([Slot::EMPTY; MAX_PTYS]);

/// Run `f` on the table with IRQs masked. Nothing inside `f` may block,
/// allocate, signal or wake — those are returned and performed afterwards.
fn with_table<R>(f: impl FnOnce(&mut [Slot; MAX_PTYS]) -> R) -> R {
    akuma_primitives::irq::with_irqs_disabled(|| f(&mut PTYS.lock()))
}

/// Run `f` on pair `n`, if it exists, and fire the wakes it leaves behind.
///
/// Every operation goes through here, so "a change wakes the waiters" is one
/// rule rather than one per call site: a caller that changed nothing wakes
/// threads that re-test and park again, which costs a context switch, where a
/// missed wake costs a hang.
fn with_pair<R>(n: u32, f: impl FnOnce(&mut PtyPair<WakeHandle>) -> R) -> Option<R> {
    let (r, wakes) = with_table(|t| {
        let pair = t.get_mut(n as usize)?.pair.as_mut()?;
        let r = f(pair);
        Some((r, pair.take_wakes()))
    })?;
    wakes.fire(super::pipe::fire_one);
    Some(r)
}

/// Encode slot `n` at `generation` as a `Process::ctty` value (`0` = none).
const fn ctty_id(n: u32, generation: u32) -> u32 {
    (generation << 8) | (n + 1)
}

/// The slot a `Process::ctty` value names, if that pair is still the one it
/// was taken from and its master is still open.
fn ctty_slot(id: u32) -> Option<u32> {
    if id == 0 {
        return None;
    }
    let n = (id & 0xFF) - 1;
    let generation = id >> 8;
    with_table(|t| {
        let s = t.get(n as usize)?;
        let pair = s.pair.as_ref()?;
        (s.generation == generation && !pair.slave_hup()).then_some(n)
    })
}

/// The pty the calling process's session controls, if any.
#[must_use]
pub fn current_ctty() -> Option<u32> {
    let proc = akuma_exec::process::current_process_shared()?;
    ctty_slot(proc.ctty.load(Ordering::Relaxed))
}

// ---- reference counts (akuma-exec runtime hooks) ---------------------------

/// `dup`/`fork` of a pty descriptor.
pub fn pty_clone_ref(n: u32, master: bool) {
    with_pair(n, |p| if master { p.master_ref() } else { p.slave_ref() });
}

/// The last reference on a descriptor went away: `close`, `dup2` over it, the
/// exec close-on-exec sweep, or the process exiting.
pub fn pty_close(n: u32, master: bool) {
    let hangup = with_table(|t| {
        let slot = t.get_mut(n as usize)?;
        let pair = slot.pair.as_mut()?;
        let hangup = if master {
            pair.master_close().then_some((pair.session, pair.fg_pgid))
        } else {
            pair.slave_close();
            None
        };
        let wakes = pair.take_wakes();
        if pair.is_unreferenced() {
            // Dropping the pair frees its buffers — inside the IRQ mask, which
            // is a `free`, not an allocation. Taken out first so the drop is the
            // last thing the locked section does.
            drop(slot.pair.take());
        }
        Some((hangup, wakes))
    });
    let Some((hangup, wakes)) = hangup else { return };
    wakes.fire(super::pipe::fire_one);
    // The terminal is gone. Linux signals the session leader and the
    // foreground group: SIGHUP, and SIGCONT so a stopped job sees it.
    if let Some((session, fg)) = hangup {
        if session > 1 {
            akuma_exec::process::deliver_signal(session, sig::SIGHUP);
            akuma_exec::process::deliver_signal(session, sig::SIGCONT);
        }
        if fg != session {
            signal_pgrp(fg, sig::SIGHUP);
            signal_pgrp(fg, sig::SIGCONT);
        }
    }
}

/// Deliver `signo` to every process (thread-group leader) in group `pgid`.
///
/// Unlike `kill_process_group` — the console's `^C`, which deliberately spares
/// the group leader — this is `killpg(2)`: the leader is a member. Group 0 and
/// group 1 are refused outright: 0 is "no foreground group", and 1 is `init`'s
/// and with it every console process, which a terminal emulator's `^C` must
/// never reach.
fn signal_pgrp(pgid: u32, signo: u32) {
    if pgid <= 1 {
        return;
    }
    // Fixed array, as in `kill_process_group`: the callback runs with IRQs
    // masked and must not allocate.
    let mut targets = [0u32; akuma_exec::process::MAX_PROCESSES];
    let mut count = 0;
    akuma_exec::process::for_each_process(|p| {
        if p.pgid == pgid && p.pid == p.tgid && p.pid > 1 && count < targets.len() {
            targets[count] = p.pid;
            count += 1;
        }
    });
    for &pid in &targets[..count] {
        akuma_exec::process::deliver_signal(pid, signo);
    }
}

// ---- open --------------------------------------------------------------------

fn install(entry: akuma_exec::process::FileDescriptor, flags: u32) -> SysResult {
    let Some(proc) = akuma_exec::process::current_process_shared() else { return Err(ESRCH) };
    let fd = proc.alloc_fd(entry);
    if flags & O_CLOEXEC != 0 {
        proc.set_cloexec(fd);
    }
    if flags & O_NONBLOCK != 0 {
        proc.set_nonblock(fd);
    }
    Ok(u64::from(fd))
}

/// `open("/dev/ptmx")`: a fresh pair, locked, and its master descriptor.
pub fn open_ptmx(flags: u32) -> SysResult {
    // The buffers are allocated outside the lock (and fallibly); the table is
    // then asked for a free slot. A full table drops the pair again.
    let Some(pair) = PtyPair::try_new() else { return Err(ENOMEM) };
    let n = with_table(|t| {
        let (n, slot) = t.iter_mut().enumerate().find(|(_, s)| s.pair.is_none())?;
        slot.generation = (slot.generation + 1) & 0x00FF_FFFF;
        slot.pair = Some(pair);
        Some(n as u32)
    });
    // `ENOSPC` is what Linux's devpts says when it runs out of indices.
    let Some(n) = n else { return Err(ENOSPC) };
    let r = install(akuma_exec::process::FileDescriptor::PtyMaster(n), flags);
    if r.is_err() {
        pty_close(n, true);
    }
    if akuma_config::SYSCALL_DEBUG_IO_ENABLED {
        akuma_primitives::safe_print!(64, "[pty] ptmx -> pair {}\n", n);
    }
    r
}

/// Take a slave reference on pair `n` as an *open*: refused while locked or
/// after the master closed.
fn slave_open_ref(n: u32) -> Result<(), u64> {
    match with_pair(n, PtyPair::slave_open) {
        None => Err(ENOENT),
        Some(Ok(())) => Ok(()),
        Some(Err(SlaveOpenError::Locked | SlaveOpenError::MasterGone)) => Err(EIO),
    }
}

/// `open("/dev/pts/N")`.
pub fn open_pts(n: u32, flags: u32) -> SysResult {
    slave_open_ref(n)?;
    let r = install(akuma_exec::process::FileDescriptor::PtySlave(n), flags);
    if r.is_err() {
        pty_close(n, false);
    }
    // `O_NOCTTY` or not, opening never makes the terminal controlling here:
    // every caller that wants one asks with `TIOCSCTTY` (musl's `login_tty`,
    // rio's spawn path, sshd). The implicit System V acquisition is what
    // `O_NOCTTY` exists to suppress, and nothing in this tree relies on it.
    r
}

/// `/dev/pts/N` → `N`, for a path that names a pair slot at all.
#[must_use]
pub fn pts_index(path: &str) -> Option<u32> {
    let digits = path.strip_prefix("/dev/pts/")?;
    if digits.is_empty() || digits.len() > 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u32 = digits.parse().ok()?;
    ((n as usize) < MAX_PTYS).then_some(n)
}

/// `open("/dev/tty")` by a process whose session a pty controls: that pty's
/// slave. `None` when the caller has no pty — the console path decides then.
pub fn open_ctty(flags: u32) -> Option<SysResult> {
    let n = current_ctty()?;
    Some(open_pts(n, flags))
}

/// What `fstat` of a pty descriptor and `stat` of its node report: Linux's
/// numbers — `/dev/ptmx` is char 5:2, a slave is char 136:N — and an inode
/// number unique per node, which is what `ttyname(3)` matches on.
#[must_use]
pub fn stat_of(master: bool, n: u32) -> Stat {
    let (ino, mode, rdev) = if master {
        (0x7074_FFFF, 0o020_666, makedev(5, 2))
    } else {
        (0x7074_0000 + u64::from(n), 0o020_620, makedev(136, u64::from(n)))
    };
    Stat { st_ino: ino, st_mode: mode, st_nlink: 1, st_rdev: rdev, st_blksize: 1024, ..Default::default() }
}

/// Does pair `n` exist (for `stat("/dev/pts/N")` and `readdir`)?
#[must_use]
pub fn pts_exists(n: u32) -> bool {
    with_table(|t| t.get(n as usize).is_some_and(|s| s.pair.is_some()))
}

// ---- read / write ---------------------------------------------------------------

/// Bytes moved per locked section. Stack-sized so neither direction
/// allocates; a master read loops while data keeps coming, so throughput does
/// not stop at this.
const CHUNK: usize = 1024;

/// Park until a wake, or until `deadline` — or for at most 10 ms when the
/// waiter set was full and this thread could not register.
fn park(registered: bool, deadline: Option<u64>) {
    let now = akuma_primitives::clock::uptime_us();
    let cap = if registered { u64::MAX } else { now + 10_000 };
    let until = deadline.unwrap_or(u64::MAX).min(cap);
    if until == u64::MAX {
        akuma_exec::threading::park_indefinitely();
    } else {
        akuma_exec::threading::schedule_blocking(until);
    }
}

fn register_current(p: &mut PtyPair<WakeHandle>) -> bool {
    let tid = akuma_exec::threading::current_thread_id();
    p.register_waiter(tid, wake_handle_for_thread(tid))
}

/// `read(2)` on either side.
pub fn read(fd_num: u32, n: u32, master: bool, buf_ptr: u64, count: usize) -> u64 {
    if count == 0 {
        return 0;
    }
    if !validate_user_ptr(buf_ptr, count) {
        return EFAULT;
    }
    if master { read_master(fd_num, n, buf_ptr, count) } else { read_slave(fd_num, n, buf_ptr, count) }
}

fn read_master(fd_num: u32, n: u32, buf_ptr: u64, count: usize) -> u64 {
    let nonblock = super::net::fd_is_nonblock(fd_num);
    let mut chunk = [0u8; CHUNK];
    let mut total = 0usize;
    loop {
        let want = (count - total).min(CHUNK);
        let Some((r, registered)) = with_pair(n, |p| match p.master_read(&mut chunk[..want]) {
            MasterRead::WouldBlock if total == 0 => (MasterRead::WouldBlock, register_current(p)),
            other => (other, false),
        }) else {
            return if total > 0 { total as u64 } else { EIO };
        };
        match r {
            MasterRead::Data(got) => {
                if copy_to_user(buf_ptr + total as u64, &chunk[..got]).is_err() {
                    return if total > 0 { total as u64 } else { EFAULT };
                }
                total += got;
                if total == count {
                    break;
                }
            }
            // Drained after some data, or the slave hung up after some data:
            // return what was read; the hangup is the next read's answer.
            _ if total > 0 => break,
            MasterRead::Hangup => return EIO,
            MasterRead::WouldBlock => {
                if nonblock {
                    super::poll::epoll_on_fd_drained(fd_num);
                    return EAGAIN;
                }
                if akuma_exec::process::should_interrupt_blocking_syscall() {
                    return EINTR;
                }
                park(registered, None);
            }
        }
    }
    super::poll::epoll_on_fd_drained(fd_num);
    total as u64
}

fn read_slave(fd_num: u32, n: u32, buf_ptr: u64, count: usize) -> u64 {
    let nonblock = super::net::fd_is_nonblock(fd_num);
    let mut chunk = [0u8; CHUNK];
    let want = count.min(CHUNK);
    let mut deadline: Option<u64> = None;
    loop {
        let expired = deadline.is_some_and(|d| akuma_primitives::clock::uptime_us() >= d);
        let Some((r, registered)) = with_pair(n, |p| match p.slave_read(&mut chunk[..want], expired) {
            b @ SlaveRead::Block { .. } => (b, register_current(p)),
            other => (other, false),
        }) else {
            return 0;
        };
        match r {
            SlaveRead::Data(got) => {
                if copy_to_user(buf_ptr, &chunk[..got]).is_err() {
                    return EFAULT;
                }
                super::poll::epoll_on_fd_drained(fd_num);
                return got as u64;
            }
            // Hung up: EOF, as Linux's hung-up tty file operations answer.
            SlaveRead::Hangup => return 0,
            SlaveRead::Block { timeout_ds } => {
                if nonblock {
                    super::poll::epoll_on_fd_drained(fd_num);
                    return EAGAIN;
                }
                if akuma_exec::process::should_interrupt_blocking_syscall() {
                    return EINTR;
                }
                if deadline.is_none()
                    && let Some(ds) = timeout_ds
                {
                    deadline = Some(akuma_primitives::clock::uptime_us() + u64::from(ds) * 100_000);
                }
                park(registered, deadline);
            }
        }
    }
}

/// `write(2)` on either side.
pub fn write(fd_num: u32, n: u32, master: bool, buf_ptr: u64, count: usize) -> u64 {
    if count == 0 {
        return 0;
    }
    if !validate_user_ptr(buf_ptr, count) {
        return EFAULT;
    }
    let nonblock = super::net::fd_is_nonblock(fd_num);
    let mut chunk = [0u8; CHUNK];
    let mut total = 0usize;
    while total < count {
        let len = (count - total).min(CHUNK);
        if copy_from_user(&mut chunk[..len], buf_ptr + total as u64).is_err() {
            return if total > 0 { total as u64 } else { EFAULT };
        }
        // `accepted` bytes of this chunk went in; `signals` were raised by them.
        let Some(r) = with_pair(n, |p| {
            let r = if master {
                p.master_write(&chunk[..len]).map(|i| (i.accepted, i.signals, p.fg_pgid))
            } else {
                p.slave_write(&chunk[..len]).map(|w| (w, akuma_pty::Signals::default(), 0))
            };
            match r {
                Ok((0, s, fg)) => (Ok((0, s, fg)), register_current(p)),
                other => (other, false),
            }
        }) else {
            return if total > 0 { total as u64 } else { EIO };
        };
        let (result, registered) = r;
        match result {
            Err(HungUp) => return if total > 0 { total as u64 } else { EIO },
            Ok((accepted, signals, fg)) => {
                for s in signals.iter() {
                    signal_pgrp(fg, s);
                }
                total += accepted;
                if accepted > 0 {
                    continue;
                }
                if total > 0 && nonblock {
                    break;
                }
                if nonblock {
                    super::poll::epoll_on_fd_drained(fd_num);
                    return EAGAIN;
                }
                if akuma_exec::process::should_interrupt_blocking_syscall() {
                    return if total > 0 { total as u64 } else { EINTR };
                }
                park(registered, None);
            }
        }
    }
    total as u64
}

// ---- poll -----------------------------------------------------------------------

/// The readiness-map state for a pty descriptor, registering the caller as a
/// waiter when it is about to sleep on the answer.
#[must_use]
pub fn poll_state(n: u32, master: bool, register_tid: Option<usize>) -> FdState {
    with_pair(n, |p| {
        if let Some(tid) = register_tid {
            // A full set only costs this poller its wake; the wait loop's own
            // periodic re-check still covers it.
            let _ = p.register_waiter(tid, wake_handle_for_thread(tid));
        }
        let s = if master { p.master_poll() } else { p.slave_poll() };
        FdState::Pty { can_read: s.readable, can_write: s.writable, hup: s.hup }
    })
    .unwrap_or(FdState::Missing)
}

// ---- ioctl ----------------------------------------------------------------------

fn put<T: Copy>(arg: u64, v: &T) -> u64 {
    if write_user_val(arg, v).is_err() { EFAULT } else { 0 }
}

fn get_i32(arg: u64) -> Result<i32, u64> {
    let mut v: i32 = 0;
    if read_user_into(&mut v, arg).is_err() {
        return Err(EFAULT);
    }
    Ok(v)
}

/// `ioctl(2)` on a pty descriptor.
///
/// `None` hands the request to the generic arms (`FIONBIO`, `FIOCLEX`,
/// `FIONCLEX`, `FIOASYNC`), which are about the descriptor rather than the
/// terminal; everything else is answered here.
pub fn ioctl(n: u32, master: bool, cmd: u32, arg: u64) -> Option<u64> {
    const FIONBIO: u32 = 0x5421;
    const FIONCLEX: u32 = 0x5450;
    const FIOCLEX: u32 = 0x5451;
    const FIOASYNC: u32 = 0x5452;
    if matches!(cmd, FIONBIO | FIONCLEX | FIOCLEX | FIOASYNC) {
        return None;
    }
    Some(match ioctl_inner(n, master, cmd, arg) {
        Ok(v) | Err(v) => v,
    })
}

fn ioctl_inner(n: u32, master: bool, cmd: u32, arg: u64) -> SysResult {
    match cmd {
        req::TCGETS => {
            let w = with_pair(n, |p| p.termios().to_wire()).ok_or(EIO)?;
            copy_to_user(arg, &w).map_err(|_| EFAULT)?;
            Ok(0)
        }
        req::TCSETS | req::TCSETSW | req::TCSETSF => {
            let mut w = [0u8; Termios::WIRE_LEN];
            copy_from_user(&mut w, arg).map_err(|_| EFAULT)?;
            let t = Termios::from_wire(&w);
            with_pair(n, |p| {
                if cmd == req::TCSETSF {
                    p.flush(true, false);
                }
                p.set_termios(t);
            })
            .ok_or(EIO)?;
            Ok(0)
        }
        req::TIOCGWINSZ => {
            let w = with_pair(n, |p| p.winsize.to_wire()).ok_or(EIO)?;
            copy_to_user(arg, &w).map_err(|_| EFAULT)?;
            Ok(0)
        }
        req::TIOCSWINSZ => {
            let mut w = [0u8; Winsize::WIRE_LEN];
            copy_from_user(&mut w, arg).map_err(|_| EFAULT)?;
            let ws = Winsize::from_wire(&w);
            let (changed, fg) = with_pair(n, |p| (p.set_winsize(ws), p.fg_pgid)).ok_or(EIO)?;
            if changed {
                signal_pgrp(fg, sig::SIGWINCH);
            }
            Ok(0)
        }
        req::TIOCGPGRP => {
            let fg = with_pair(n, |p| p.fg_pgid).ok_or(EIO)?;
            Ok(put(arg, &(fg as i32)))
        }
        req::TIOCSPGRP => {
            let pgid = get_i32(arg)?;
            if pgid <= 0 {
                return Err(EINVAL);
            }
            with_pair(n, |p| p.fg_pgid = pgid as u32).ok_or(EIO)?;
            Ok(0)
        }
        req::TIOCGSID => {
            let sid = with_pair(n, |p| p.session).ok_or(EIO)?;
            if sid == 0 {
                return Err(ENOTTY);
            }
            Ok(put(arg, &(sid as i32)))
        }
        req::TIOCSCTTY => set_ctty(n, arg == 1),
        req::TIOCNOTTY => {
            let proc = akuma_exec::process::current_process_shared().ok_or(ESRCH)?;
            let id = proc.ctty.load(Ordering::Relaxed);
            if ctty_slot(id) != Some(n) {
                return Err(ENOTTY);
            }
            proc.ctty.store(0, Ordering::Relaxed);
            let pid = proc.pid;
            with_pair(n, |p| {
                if p.session == pid {
                    p.session = 0;
                    p.fg_pgid = 0;
                }
            });
            Ok(0)
        }
        req::FIONREAD => {
            let q = with_pair(n, |p| if master { p.output_queued() } else { p.input_queued() }).ok_or(EIO)?;
            Ok(put(arg, &(q as i32)))
        }
        req::TIOCOUTQ => {
            let q = with_pair(n, |p| if master { p.input_queued() } else { p.output_queued() }).ok_or(EIO)?;
            Ok(put(arg, &(q as i32)))
        }
        req::TCFLSH => {
            // The master's input is the slave's output and vice versa.
            let (input, output) = match arg {
                req::TCIFLUSH => (true, false),
                req::TCOFLUSH => (false, true),
                req::TCIOFLUSH => (true, true),
                _ => return Err(EINVAL),
            };
            let (input, output) = if master { (output, input) } else { (input, output) };
            with_pair(n, |p| p.flush(input, output)).ok_or(EIO)?;
            Ok(0)
        }
        // `tcdrain` (`TCSBRK` with 1), breaks, and flow control: there is no
        // line to drain or break and no output stop, so all succeed.
        req::TCSBRK | req::TCSBRKP | req::TCXONC => Ok(0),
        req::TIOCPKT => {
            // Packet mode is not implemented; turning it off is a no-op.
            if get_i32(arg)? == 0 { Ok(0) } else { Err(EINVAL) }
        }
        req::TIOCGPTN if master => Ok(put(arg, &n)),
        req::TIOCSPTLCK if master => {
            let v = get_i32(arg)?;
            with_pair(n, |p| p.set_locked(v != 0)).ok_or(EIO)?;
            Ok(0)
        }
        req::TIOCGPTLCK if master => {
            let locked = with_pair(n, |p| p.is_locked()).ok_or(EIO)?;
            Ok(put(arg, &i32::from(locked)))
        }
        req::TIOCGPTPEER if master => {
            // `arg` is the open flags, `O_RDWR|O_NOCTTY` plus perhaps
            // `O_CLOEXEC` / `O_NONBLOCK` — asm-generic encoding on both
            // kernels for those three bits.
            open_pts(n, arg as u32)
        }
        _ => Err(ENOTTY),
    }
}

/// `TIOCSCTTY`: make pair `n` the calling process's controlling terminal.
///
/// Linux allows it to a session leader without one. This is laxer on the
/// first condition — `setsid` has only just become real on amd64 and nothing
/// here tracks session ids beyond the pair's own — and strict on the one that
/// protects anybody: a terminal that already controls a *live* session is
/// refused (`EPERM`) unless `steal` (arg 1) is asked for.
fn set_ctty(n: u32, steal: bool) -> SysResult {
    let proc = akuma_exec::process::current_process_shared().ok_or(ESRCH)?;
    let pid = proc.pid;
    let pgid = proc.pgid;
    let (generation, owner) = with_table(|t| {
        let s = t.get(n as usize)?;
        let p = s.pair.as_ref()?;
        Some((s.generation, p.session))
    })
    .ok_or(EIO)?;
    let id = ctty_id(n, generation);
    if owner != 0 && owner != pid && !steal {
        let owner_live = akuma_exec::process::lookup_process_shared(owner)
            .is_some_and(|o| o.ctty.load(Ordering::Relaxed) == id);
        if owner_live {
            return Err(EPERM);
        }
    }
    with_pair(n, |p| {
        p.session = pid;
        p.fg_pgid = pgid;
    })
    .ok_or(EIO)?;
    proc.ctty.store(id, Ordering::Relaxed);
    Ok(0)
}

// ---- process groups and sessions --------------------------------------------------
//
// The amd64 kernel answered `getpgid`/`getsid`/`getpgrp` with 1 and made
// `setpgid`/`setsid` no-ops, deliberately: its console's `TerminalState` keeps
// `foreground_pgid` at the spawned child, and a shell that read a real group
// and `TIOCSPGRP`'d it would move the console's `^C` target
// (`amd64/src/usermode.rs`, the `Getpgid` arm). A pty session is a terminal
// with its own foreground group, and a shell on it does job control —
// busybox `ash` loops `killpg(0, SIGTTIN)` until `tcgetpgrp() == getpgrp()`.
// So the answers are real **for a process whose session a pty controls**, and
// unchanged for everything else.

/// `setsid()`: a new session and process group, no controlling terminal.
/// Real on both kernels; returns the new session id (the caller's pid).
pub fn sys_setsid() -> u64 {
    akuma_exec::process::with_current_process(|p| {
        p.pgid = p.pid;
        p.ctty.store(0, Ordering::Relaxed);
        u64::from(p.pid)
    })
    .unwrap_or(ESRCH)
}

fn target_pid(pid: u32) -> Option<u32> {
    if pid == 0 { akuma_exec::process::read_current_pid() } else { Some(pid) }
}

/// amd64's `getpgid(pid)` / `getpgrp()`: the real group inside a pty session,
/// 1 elsewhere.
pub fn sys_getpgid_gated(pid: u32) -> u64 {
    let Some(p) = target_pid(pid).and_then(akuma_exec::process::lookup_process_shared) else {
        return ESRCH;
    };
    if ctty_slot(p.ctty.load(Ordering::Relaxed)).is_some() { u64::from(p.pgid) } else { 1 }
}

/// amd64's `getsid(pid)`: the controlling pty's session leader, 1 elsewhere.
pub fn sys_getsid_gated(pid: u32) -> u64 {
    let Some(p) = target_pid(pid).and_then(akuma_exec::process::lookup_process_shared) else {
        return ESRCH;
    };
    match ctty_slot(p.ctty.load(Ordering::Relaxed)) {
        Some(n) => with_pair(n, |pair| u64::from(pair.session)).unwrap_or(1),
        None => 1,
    }
}

/// amd64's `setpgid(pid, pgid)`: real for a caller in a pty session, the old
/// accepted no-op elsewhere.
pub fn sys_setpgid_gated(pid: u32, pgid: u32) -> u64 {
    if current_ctty().is_some() { super::proc::sys_setpgid(pid, pgid) } else { 0 }
}

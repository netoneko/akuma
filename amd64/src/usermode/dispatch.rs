//! The amd64 syscall dispatcher: one x86_64 number in, one result out.
//!
//! Moved out of `usermode.rs` 2026-11-10 and split by family (W2 of
//! `docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`). Every arm and its comment moved
//! unchanged; what changed is that the shared `Syscall` table, which was one
//! ~900-line `match`, is now eight family functions tried in a fixed order.
//! The stages, in the order a number meets them:
//!
//! 1. **Akuma-private** numbers (`0x1000 +` the AArch64 number) — [`akuma_private`].
//! 2. **x86-only legacy spellings** (no `asm-generic` twin), each narrowed to a
//!    modern call — inline in [`syscall_dispatch`].
//! 3. **Shared-number** calls (same number on every 64-bit Linux) — inline.
//! 4. Everything else, decoded to `akuma_syscalls_abi::Syscall` and offered to
//!    each family: [`fs`], [`poll`], [`mem`], [`net`], [`process`], [`signal`],
//!    [`time`], [`system`]. A family answers `None` for a call it does not own.
//!
//! **A variant belongs to exactly one family**, and all of its arms — a guarded
//! `AF_UNIX` arm and the unguarded one after it — stay together in that
//! family's `match`, in their original order. That is what keeps the split
//! behaviour-preserving: the families are disjoint, so the order they are tried
//! in cannot change which arm answers. Adding a row to `akuma-syscalls-abi`
//! means adding its arm to one family; a decoded call no family claims prints
//! the `has no dispatch arm` line and returns `ENOSYS`, as before.
//!
//! **The families are `#[inline(always)]`, and tried by `if let .. return`, not
//! `Option::or_else`.** Both are what let LLVM fold eight `match`es on the same
//! `call` back into the dispatcher's own switch: the hot path is one function with
//! five jump tables (it had three as a single `match`), so a call pays at most a
//! couple of range checks for the split. Left to its own judgement LLVM keeps all
//! eight out of line (each is called once, but `fs` is 2 KB), and with `or_else`
//! the closures stay out of line even when the families are inlined into them.
//!
//! [`to_glue`] / [`to_glue_raw`] are the C1 seam to `akuma-syscalls-glue` and
//! live here because nothing else should need them; the boot self-tests reach
//! them as `super::dispatch::to_glue`.

use super::*;
use crate::fd::errno;

/// `AT_FDCWD` — "relative to the current directory". Every process here
/// starts at `/` and only `chdir` moves it, so for most of the process's
/// life this is the root; the resolution against `Process::cwd` happens in
/// `akuma_syscalls_glue::fs::resolve_path_at`, not here. What every legacy,
/// non-`at` path syscall below passes to its `*at` implementation.
const AT_FDCWD: u64 = (-100i64) as u64;

/// How long a `resolve_host` may take before it gives up. Generous: a
/// cold cache on this target means a real UDP round trip to whatever
/// `/etc/resolv.conf`'s first reachable server is, and the alternative to
/// waiting is a program that reports "no such host" for a working name.
const DNS_TIMEOUT_US: u64 = 5_000_000;

/// Akuma's own syscalls, before the Linux table.
///
/// They live at `0x1000 +` their AArch64 number (see `libakuma`'s
/// `AKUMA_PRIVATE_BASE`), far above any allocated Linux number. Checking this
/// range first is what keeps a shell's keystroke poll from dispatching into
/// whatever Linux happens to have at 313 — `finit_module`, as it turns out.
const AKUMA_PRIVATE_BASE: u64 = 0x1000;

pub(super) fn syscall_dispatch(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) -> u64 {
    if nr >= AKUMA_PRIVATE_BASE {
        return akuma_private(nr, [a1, a2, a3, a4, a5, a6]);
    }

    // The **x86-only legacy spellings**, and nothing else.
    //
    // Each of these numbers exists on x86_64 and has no `asm-generic` twin, so
    // `akuma_syscalls_abi::Syscall` deliberately cannot name it (rule 2 in that
    // crate's header): giving one an aarch64 number would mean inventing a fact
    // about Linux. They are handled here, ahead of the neutral table, and every
    // one of them narrows to a modern call the neutral table *does* name —
    // usually by supplying the `AT_FDCWD` that the `*at` form wants.
    //
    // Not redundant with the `*at` arms below: musl issues whichever spelling
    // the architecture has, and x86_64 has both, so which one arrives is a
    // property of the *caller*. busybox `mkdir` uses 83 and got `ENOSYS` while
    // `mkdirat` sat implemented and working — reported as `mkdir: can't create
    // directory: Function not implemented`, which reads as a filesystem that
    // cannot make directories rather than a dispatch table missing a number.
    //
    // **Adding an arm here is a claim that the call is x86-only.** Check
    // `akuma_syscalls_abi`'s table first; if the call has an asm-generic number,
    // it belongs in the `match call` below, where the AArch64 kernel's
    // implementation can eventually serve it.
    // The ten legacy path spellings (82/83/84/86/87/88/89/90/92/94) are
    // `AT_FDCWD` folds owned by `akuma_syscalls_abi::legacy` - one table for
    // this kernel and Taoxichuan; the symlink/utimensat history is its doc.
    if let Some((call, args)) = akuma_syscalls_abi::legacy::fold(nr, [a1, a2, a3, a4, a5, a6]) {
        return to_glue(call, args);
    }
    match nr {
        // x86_64 158: the TLS-base primitive. No aarch64 number, and no
        // equivalent either — `arch_prctl(ARCH_SET_FS)` is what `set_tpidr_el0`
        // is on the other side, and that one is Akuma-private.
        158 => return sys_arch_prctl(a1, a2),
        // Path-based `struct stat`. `stat` (4) and `lstat` (6) are x86-only —
        // `asm-generic` dropped them — and both narrow to `newfstatat`, which
        // is in the neutral table. `stat` follows a final symlink, `lstat` does
        // not (`AT_SYMLINK_NOFOLLOW` == 0x100); on this target that changes
        // nothing (see `fd::sys_newfstatat`). `AT_FDCWD` is -100. busybox `sh`
        // stats every PATH entry before it will run an applet — without this it
        // saw `ENOSYS` and reported "Function not implemented" for a working
        // builtin.
        4 => return crate::fd::sys_newfstatat(AT_FDCWD, a1, a2, 0),
        6 => return crate::fd::sys_newfstatat(AT_FDCWD, a1, a2, 0x100),
        // `open(path, flags, mode)` — x86_64 2. x86_64 musl issues this directly
        // (it only falls back to `openat` on architectures without `open`, like
        // aarch64), so `busybox cat` hit `ENOSYS` here until now. `openat`
        // resolves the relative path against `Process::cwd` (the root until a
        // `chdir` moves it), which is what `AT_FDCWD` means.
        2 => return crate::fd::sys_openat(AT_FDCWD, a1, a2, a3),
        // `access(path, mode)` — existence only. This target has one user (root)
        // and no per-file exec tracking worth trusting, so "the path resolves"
        // is the honest answer; a real permission check would be a guess. The
        // `faccessat` spelling is in the neutral table and answers identically.
        // `access(path, mode)` is `faccessat(AT_FDCWD, path, mode)` — glue's arm.
        21 => return to_glue(Syscall::Faccessat, [AT_FDCWD, a1, a2, 0, 0, 0]),
        // `poll(fds, nfds, timeout_ms)` — x86_64 7, narrowing to the same core
        // `ppoll` uses. An interactive `busybox sh` polls its stdin on every
        // keystroke; `ENOSYS` here was a forever-loop of "sh: poll: Function
        // not implemented".
        7 => return crate::fd::sys_poll(a1, a2, a3),
        // `select(nfds, readfds, writefds, exceptfds, timeout)` — x86_64 23.
        // asm-generic has only `pselect6`. `apk` waits for post-connect socket
        // writability through this syscall; `ENOSYS` here wedged its TLS fetch
        // mid-handshake (see `fd::sys_select`).
        23 => return crate::fd::sys_select(a1, a2, a3, a4, a5),
        // `pipe` (22) and `dup2` (33) — the legacy halves of the four calls that
        // make a shell a shell (`pipe2`/`dup3` are the neutral pair below).
        // Every one of them was `ENOSYS` until 2026-09-06, which is why
        // `cmd | cmd` reported *can't create pipe* and `echo x > file` left a
        // zero-length file: a shell builds both out of `pipe` plus `dup2`, and
        // neither existed.
        22 => return crate::fd::sys_pipe2(a1, 0),
        33 => return crate::fd::sys_dup2(a1, a2),
        // `fork` (57) / `vfork` (58) — a real eager-copy fork; see `sys_fork`
        // (`vfork` gets the same, its "don't touch the parent" contract is moot
        // once the address space is copied). asm-generic has neither: `clone`
        // with `CLONE_VM` clear is the only spelling there, and it is in the
        // neutral table below.
        57 | 58 => return sys_fork(),
        // `getpgrp()` — x86_64 111, x86-only; asm-generic callers use
        // `getpgid(0)`, and so does this: 1 outside a pty session, the real
        // group inside one (see the `Getpgid` arm).
        111 => return akuma_syscalls_glue::pty::sys_getpgid_gated(0),
        // The x86-only halves of the clock. `clock_settime` (227) and
        // `adjtimex` (159) are in the neutral table; these two are not, and
        // `busybox ntpd -q` reaches for whichever musl offers.
        //
        // Why it matters beyond `date` being wrong: at the epoch **every TLS
        // certificate on earth is not-yet-valid**, and `apk` reports that as
        // `server certificate not trusted` — sending you to look at the CA
        // bundle, which is fine.
        164 => {
            // `settimeofday(tv, tz)`. `tz` is ignored: it has been meaningless
            // since the 1980s and Linux itself does nothing useful with it.
            if a1 == 0 {
                return 0;
            }
            let Some(tv) = crate::uaccess::read_val::<akuma_syscalls_linux::time::Timeval>(a1)
            else {
                return errno::EFAULT;
            };
            if tv.tv_sec < 0 || tv.tv_usec < 0 || tv.tv_usec >= 1_000_000 {
                return errno::EINVAL;
            }
            crate::clock::set_unix_us(
                (tv.tv_sec.cast_unsigned()).saturating_mul(1_000_000) + tv.tv_usec.cast_unsigned(),
            );
            return 0;
        }
        // `alarm(seconds)` — x86_64 37, x86-only. asm-generic has no `alarm`,
        // so musl on *that* architecture spells `alarm(3)` as a `setitimer`
        // pair and the other kernel never sees this number; here musl emits
        // the raw syscall, and it was `ENOSYS` until C3.
        //
        // Served by the crate that owns the itimer state rather than by a
        // local copy: it takes and returns plain seconds, so no `itimerval`
        // has to be marshalled through user memory to reach the same three
        // atomics `setitimer` writes.
        37 => return akuma_syscalls_glue::sys_alarm(a1),
        // `pause()` — x86_64 34, x86-only, and `alarm`'s other half: musl
        // emits the raw syscall here and `ppoll(0,0,0,0)` on asm-generic, so
        // the other kernel has never needed a number for it and this one
        // answered `ENOSYS` to the oldest idiom in POSIX. Found by
        // `/probes/clockprobe` rung 8, which armed an `alarm` correctly and
        // then did not wait for it.
        34 => return akuma_syscalls_glue::sys_pause(),
        // `gettimeofday(*timeval, *timezone)` — x86_64 96. `timezone` (a2)
        // is always NULL from every real caller and is not consulted.
        96 => {
            if a1 != 0 {
                let us = crate::clock::realtime_us();
                // A user `struct timeval { i64 tv_sec, i64 tv_usec }`.
                let tv = [(us / 1_000_000).cast_signed(), (us % 1_000_000).cast_signed()];
                if !crate::uaccess::write_val(a1, tv) {
                    return errno::EFAULT;
                }
            }
            return 0;
        }
        // `time(*time_t)` — x86_64 201. Writes through `tloc` when non-null,
        // in addition to the return value, matching `time(2)`'s own contract.
        201 => {
            let secs = (crate::clock::realtime_us() / 1_000_000).cast_signed();
            if a1 != 0 && !crate::uaccess::write_val::<i64>(a1, secs) {
                return errno::EFAULT;
            }
            return secs as u64;
        }
        // `epoll_create(size)` — x86_64 213, x86-only (`epoll_create1` is the
        // asm-generic spelling and the one in the neutral table). `size` has
        // been advisory since Linux 2.6.8 and every kernel including this one
        // ignores it; musl's own `epoll_create` narrows to `epoll_create1(0)`
        // the same way, so this arm exists for a caller that issues 213 by
        // hand rather than for anything musl emits.
        213 => return to_glue(Syscall::EpollCreate1, [0, 0, 0, 0, 0, 0]),
        // `epoll_wait(epfd, events, maxevents, timeout)` — x86_64 232,
        // x86-only (asm-generic only ever had `epoll_pwait`). Narrows to it
        // with a null sigmask/sigsetsize, which is exactly what
        // `epoll_wait(2)` already means — glue's `sys_epoll_pwait` never reads
        // those two arguments regardless, so there is no signal-mask feature
        // silently lost by treating the two spellings alike.
        232 => return to_glue(Syscall::EpollPwait, [a1, a2, a3, a4, 0, 0]),
        _ => {}
    }

    // The **shared-number** syscalls, and nothing else.
    //
    // A third category next to the x86-only legacy list above and the neutral
    // table below: every syscall Linux added since roughly 5.1 was given the
    // same number on every 64-bit architecture, x86_64 included, so there is
    // no vocabulary to hop — `akuma_syscalls_abi::Syscall` deliberately cannot
    // name one (rule 3 in that crate's header, would fail
    // `tables_disagree_where_linux_does` by construction). Dispatched straight
    // to glue by the one number both sides already agree on.
    //
    // **Adding an arm here is a claim that the number is identical on both
    // architectures.** Check the real x86_64 and asm-generic numbers (musl's
    // own `bits/syscall.h` for each target is the authoritative source used
    // here) before adding one; getting this wrong silently reaches a
    // *different* handler on one architecture, the same failure mode C1's
    // vocabulary hop exists to prevent.
    match nr {
        // `pidfd_open(pid, flags)` — 434 on both x86_64 and asm-generic.
        434 if NO_PIDFD.load(Ordering::Relaxed) => return (-38i64) as u64,
        434 => return to_glue_raw(akuma_syscalls_linux::nr::PIDFD_OPEN, [a1, a2, 0, 0, 0, 0]),
        // `pidfd_send_signal(pidfd, sig, info, flags)` — 424 on both. `info`
        // is not decoded here or in glue — see `sys_pidfd_send_signal`'s own
        // doc comment for why the siginfo payload has nowhere to land.
        424 => return to_glue_raw(akuma_syscalls_linux::nr::PIDFD_SEND_SIGNAL, [a1, a2, a3, a4, 0, 0]),
        _ => {}
    }

    // Everything else goes through `akuma_syscalls_abi::Syscall` — the
    // architecture-neutral name, decoded from the x86_64 number here and
    // encodable back to the asm-generic one `akuma-syscalls-glue` dispatches on.
    //
    // That second half is the point: C1 folds these arms into glue one at a
    // time (`proposals/NEXT_AGENT_AMD64_C1_USERMODE_FOLD.md`), and glue's table
    // is asm-generic. Handing it `1` meaning `write` would find the *wrong*
    // handler rather than none — so the vocabulary hop happens once, here,
    // through a table whose two halves are round-trip tested against each other.
    // x86_64 nr 26 is `msync` — routed **before** the shared table to this
    // kernel's own `mm::sys_msync` rather than glue's. (asm-generic does have
    // msync, 227: `Syscall::Msync` is a row since 2026-11-01, and a comment
    // here said otherwise until then.) See `mm::sys_msync` for
    // what it does and `MmapRegion::shared_write` for why it exists at all:
    // it is the flush `memmap2::MmapMut::flush` compiles to, and without it a
    // writable `MAP_SHARED` mapping could not be trusted at close.
    if nr == 26 {
        return crate::mm::sys_msync(a1, a2);
    }
    let Some(call) = Syscall::from_x86_64(nr) else {
        report_unknown_syscall(nr);
        return errno::ENOSYS;
    };

    let a = [a1, a2, a3, a4, a5, a6];
    if let Some(r) = fs(call, a) {
        return r;
    }
    if let Some(r) = poll(call, a) {
        return r;
    }
    if let Some(r) = mem(call, a) {
        return r;
    }
    if let Some(r) = net(call, a) {
        return r;
    }
    if let Some(r) = process(call, a) {
        return r;
    }
    if let Some(r) = signal(call, a) {
        return r;
    }
    if let Some(r) = time(call, a) {
        return r;
    }
    if let Some(r) = system(call, a) {
        return r;
    }
    // Decoded, but no family has an arm for it. Linux has no such answer,
    // so say so: a row added to `akuma-syscalls-abi` without its arm here
    // used to fall in silently (`brk` did, and earlier `fchdir`), where an undecoded
    // number at least prints `no row for`.
    akuma_primitives::safe_print!(
        160,
        "[syscall] x86_64 nr={nr} decodes to {call:?} but has no dispatch arm — returning ENOSYS\n"
    );
    errno::ENOSYS
}

/// Akuma's own syscalls: `nr` is `AKUMA_PRIVATE_BASE +` the AArch64 number.
fn akuma_private(nr: u64, a: [u64; 6]) -> u64 {
    let [a1, a2, a3, a4, a5, a6] = a;
    match nr - AKUMA_PRIVATE_BASE {
        // `spawn(path, argv, envp, stdin, stdin_len, flags)` — six
        // arguments, and the sixth is the one `sshd` sets for a client's
        // `pty-req`. It used to stop here: this arm passed five and the
        // comment claimed `syscall_entry` had dropped the flags word, which
        // it never did (it pushes `r9` precisely so a sixth argument
        // survives — `futex`'s bitset is the other user). So `SPAWN_FLAG_PTY`
        // was not "accepted and ignored", it was not received.
        301 => sys_spawn(a1, a2, a3, a4, a5, a6),
        // `resolve_host(name_ptr, name_len, out4)` — Akuma's own 300.
        //
        // Wired 2026-09-06 because `hget` (and anything else built on
        // `libakuma-tls`) resolves through this, not through musl: it is a
        // `no_std` binary with no resolver of its own. Without it, TLS on
        // this target failed at `DNS resolution failed` while `busybox
        // wget http://…` resolved the same name perfectly — because
        // busybox is musl and goes out over UDP itself.
        //
        // The kernel already had the resolver; `clock.rs`'s SNTP bootstrap
        // uses the same `akuma_net::dns::resolve_ipv4`. Only the syscall was
        // missing.
        300 => {
            // A hostname is passed as `(ptr, len)` and is **not**
            // NUL-terminated, so `read_cstr` is the wrong tool — it would
            // run past the end looking for a terminator that is not there.
            const MAX_HOST: u64 = 255; // RFC 1035's limit on a domain name
            if a2 == 0 || a2 > MAX_HOST {
                return errno::EINVAL;
            }
            let mut buf = [0u8; MAX_HOST as usize];
            let n = a2 as usize;
            if !crate::uaccess::read_bytes(a1, &mut buf[..n]) {
                return errno::EFAULT;
            }
            let Ok(name) = core::str::from_utf8(&buf[..n]) else {
                return errno::EINVAL;
            };
            let Ok(ip) = akuma_net::dns::resolve_ipv4(name, DNS_TIMEOUT_US).map(|ip| ip.octets()) else {
                // `EAI_FAIL`'s errno cousin: the name did not resolve. Not
                // `EFAULT` — the caller's pointers were fine.
                return errno::ENOENT;
            };
            if !crate::uaccess::write_bytes(a3, &ip) {
                return errno::EFAULT;
            }
            0
        }
        // `poll_input_event(buf, len, timeout_us)` — Akuma-private 313.
        //
        // **Glue's arm whenever this process has a channel**, which since
        // the console channel landed is every process an ssh session or the
        // serial console runs. It has to be: `crate::fd`'s local copy reads
        // the **UART** (`crate::input::getb`), and an ssh session's
        // keystrokes are in its `ProcessChannel`, not on the serial line —
        // so `paws` printed its prompt and then ignored everything typed at
        // it, and `meow`'s TUI blocked on its terminal-size probe before it
        // ever painted a frame. Both looked like the program hanging.
        // Glue's version also honours `timeout_us`, which the local one
        // takes and ignores (`_timeout_us`) — and that timeout is what a
        // TUI's frame tick *is*: `meow` polls with 50 ms and redraws when
        // it expires, so an untimed wait freezes the screen between
        // keystrokes even once the bytes arrive.
        //
        // The local arm stays for the two cases glue cannot serve: a
        // redirected fd 0 that is a pipe, and a process with no channel at
        // all (glue answers `ENOMEM` for that, which would be a regression
        // rather than a fix).
        313 => {
            if akuma_exec::process::current_channel().is_some() {
                to_glue_raw(akuma_syscalls_linux::nr::POLL_INPUT_EVENT, [a1, a2, a3, 0, 0, 0])
            } else {
                crate::fd::sys_poll_input_event(a1, a2, a3)
            }
        }
        303 => sys_waitpid(a1, a2, a3),
        // The **terminal family** — Akuma-private 307-312 and 314, all of
        // them glue's, all of them `ENOSYS` here until 2026-09-19.
        //
        // That is what the `meow` TUI screenshot was: `set_cursor_position`
        // returned `ENOSYS`, so the footer printed **wherever the cursor
        // happened to be** and every 50 ms frame tick appended another copy
        // of it down the screen instead of repainting one row. With
        // `set_terminal_attributes` missing too the session never left
        // cooked mode, so input was line-buffered and echoed by the kernel
        // on top of that. A full-screen program cannot work without these;
        // it can only *appear* to, which is how a harness that reads the
        // transcript scores it as a pass.
        //
        // Every one has a real implementation in `term.rs` that reads or
        // writes this process's `ProcessChannel` and its `TerminalState` —
        // the same pair arm 313 was folded onto above, and the same reason:
        // on this target the channel is what `sshd` and the console both
        // speak, and glue is where the line discipline lives. Gated on the
        // channel for 313's reason: without one there is no terminal, and
        // answering `ENOSYS` there is what these did yesterday.
        307 | 308 | 309 | 310 | 311 | 312 | 314
            if akuma_exec::process::current_channel().is_some() =>
        {
            to_glue_raw(nr - AKUMA_PRIVATE_BASE, [a1, a2, a3, 0, 0, 0])
        }
        // `set_terminal_attributes` on a pty descriptor: a process whose
        // stdin is a pty slave (the `ssh` client run in a terminal
        // emulator) has no console channel, and the arm above answered it
        // `ENOSYS` — so the slave stayed cooked. Glue's arm acts on the
        // pty's own termios when the fd is one.
        307 if akuma_syscalls_glue::pty::is_pty_fd(a1) => {
            to_glue_raw(nr - AKUMA_PRIVATE_BASE, [a1, a2, a3, 0, 0, 0])
        }
        // `uptime()` — microseconds since boot, matching
        // `akuma_syscalls_time::sys_uptime` on the AArch64 side. `herd`'s
        // whole supervision loop is keyed on it (restart delays, start
        // delays, the 20 s config reload), so without this its clock
        // reads `-ENOSYS` as a colossal timestamp and every delay is
        // already in the past.
        319 => crate::net::uptime_us(),
        326 => sys_close_child_stdin(a1),
        // `kill(pid, sig)` — Akuma-private 302, what libakuma's
        // `kill_signal` issues. Glue's `nr::AKUMA_KILL` arm is the same
        // `proc::sys_kill` that Linux `kill` (62, `Syscall::Kill` below)
        // reaches, so the two spellings deliver identically.
        //
        // This was `302 => 0` — a no-op success, from when this target had
        // nothing to deliver a signal to — until 2026-09-24. So `herd stop`
        // and `herd disable` never stopped a service here: herd dropped the
        // pid and the process ran on unsupervised
        // (`docs/archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md` §6 item 7).
        // sshd's session-teardown SIGHUP/SIGTERM is now real too, which is
        // the Linux behaviour.
        302 => to_glue_raw(akuma_syscalls_linux::nr::AKUMA_KILL, [a1, a2, 0, 0, 0, 0]),
        // `console_notify(ptr, len)` — Akuma-private 322, served on this
        // target only. Prints a
        // caller-supplied line straight to the framebuffer/serial console,
        // framed so it stands out in a scrolling boot log. The HP box has a
        // screen and no keyboard, so this is how a program — or a person
        // driving one over ssh — puts a word in front of whoever is watching
        // that screen. `/bin/wall` is the userspace front end.
        //
        // Hard-capped at 512 bytes, and every byte below space (bar `\t`) is
        // rendered as '.': the framebuffer console interprets some control
        // bytes, and a careless or hostile caller should not be able to move
        // its cursor or scroll it from here. A trailing '\n' the caller
        // included is dropped — the framing adds its own.
        322 => {
            const MAX_MSG: usize = 512;
            let n = (a2 as usize).min(MAX_MSG);
            if n == 0 {
                return errno::EINVAL;
            }
            let mut buf = [0u8; MAX_MSG];
            if !crate::uaccess::read_bytes(a1, &mut buf[..n]) {
                return errno::EFAULT;
            }
            let mut end = n;
            while end > 0 && (buf[end - 1] == b'\n' || buf[end - 1] == b'\r') {
                end -= 1;
            }
            for b in &mut buf[..end] {
                if (*b < 0x20 && *b != b'\t') || *b == 0x7f {
                    *b = b'.';
                }
            }
            let msg = core::str::from_utf8(&buf[..end]).unwrap_or("<console_notify: not UTF-8>");
            serial::puts("\n>>> ");
            serial::puts(msg);
            serial::puts(" <<<\n");
            0
        }
        // The box family — Akuma-private 315-318/324/325, forwarded raw to
        // `akuma-syscalls-glue` (`sc-containers`, on by default here since
        // the `mount`/`umount2` pass). Every one of these already has a
        // real, arch-neutral implementation the AArch64 kernel's `box`/
        // `herd` use today; nothing here does anything but hand the same
        // six raw argument words across. `fork_process`'s
        // `Process::inherit_from` already copies `box_id`/`namespace` from
        // parent to child on **this** target too (shared `akuma-exec`
        // code, unmodified), so a process born into a box via `spawn_ext`
        // stays in it across every `fork`/`clone` its descendants make —
        // there was no separate propagation step to add.
        //
        // `spawn_ext(path, options_ptr, options_len)` — `SpawnOptions`'s
        // `box_id` field is what a boxed process is actually born with;
        // plain `sys_spawn`(301) above always births box 0, on purpose,
        // and stays that way.
        315 => to_glue_raw(akuma_syscalls_linux::nr::SPAWN_EXT, [a1, a2, a3, 0, 0, 0]),
        // `register_box(id, name_ptr, name_len, root_ptr, root_len, primary_pid)`.
        316 => to_glue_raw(akuma_syscalls_linux::nr::REGISTER_BOX, [a1, a2, a3, a4, a5, a6]),
        // `kill_box(box_id)`.
        317 => to_glue_raw(akuma_syscalls_linux::nr::KILL_BOX, [a1, 0, 0, 0, 0, 0]),
        // `reattach(pid, force)` — `box grab`'s terminal-takeover primitive.
        318 => to_glue_raw(akuma_syscalls_linux::nr::REATTACH, [a1, a2, 0, 0, 0, 0]),
        // `set_box_stack(box_id, stack)` — `stack == 1` (NetBSD rump) is a
        // real no-op refusal on this target: amd64 has no `akuma-rump`
        // dependency at all, so nothing on this kernel can ever answer as
        // a rump box's network stack. `stack == 0` (smoltcp, the only
        // stack amd64 has) is a genuine accept.
        324 => to_glue_raw(akuma_syscalls_linux::nr::SET_BOX_STACK, [a1, a2, 0, 0, 0, 0]),
        // `mount_in_ns(box_id, target_ptr, target_len, fstype_ptr, fstype_len, data_ptr)`
        // — composes a box's namespace mounts from box 0, before anything
        // runs in it. See `container.rs`'s own header for why this is
        // never a boxed process's own call to make.
        325 => to_glue_raw(akuma_syscalls_linux::nr::MOUNT_IN_NS, [a1, a2, a3, a4, a5, a6]),
        _ => errno::ENOSYS,
    }
}

/// Files, descriptors, paths, mounts and `io_*` AIO.
#[allow(clippy::inline_always)] // one switch, not eight calls: see the module doc
#[inline(always)]
fn fs(call: Syscall, a: [u64; 6]) -> Option<u64> {
    let [a1, a2, a3, a4, a5, a6] = a;
    Some(match call {
        Syscall::Write => sys_write(a1, a2, a3),
        Syscall::Read => crate::fd::sys_read(a1, a2, a3),
        // `pread64(fd, buf, count, offset)` — a read that does not move the
        // cursor. Not dispatched at all until 2026-09-07: every `pread` on this
        // target was `ENOSYS`, which `mmapsum`'s reference arm hit at offset 0.
        Syscall::Pread64 => crate::fd::sys_pread64(a1, a2, a3, a4),
        // `ftruncate(fd, length)`. The linker's output file is created by
        // opening it, sizing it with this, and mapping the result; without it
        // `rust-lld` reported `cannot open output file` for a file it had
        // already opened successfully.
        Syscall::Ftruncate => to_glue(call, [a1, a2, 0, 0, 0, 0]),
        Syscall::Truncate => to_glue(call, [a1, a2, 0, 0, 0, 0]),
        // `fallocate(fd, mode, offset, len)` — x86_64 285, row added
        // 2026-10-08 (Chromium's shared-memory files; 82 `ENOSYS` per run).
        // Same argument order as glue's arm; mode 0 grows the file.
        Syscall::Fallocate => to_glue(call, [a1, a2, a3, a4, 0, 0]),
        // ── 2026-09-12: the batch the in-guest `cargo` asked for ───────────
        //
        // Every one of these had a number in `akuma-syscalls-linux` and an arm
        // in `akuma-syscalls-glue`, and no row in `akuma-syscalls-abi`. They
        // were found the way the last three were — by the console naming the
        // number (`[syscall] no row for x86_64 nr=…`) while `cargo` ran — and
        // none of them needed an implementation written.
        //
        // `pwrite64` is the one with a visible symptom: **SQLite writes with
        // it**, cargo's global cache is a SQLite database, and every in-guest
        // cargo invocation opened with `disk I/O error / Error code 778`.
        Syscall::Pwrite64 => to_glue(call, [a1, a2, a3, a4, 0, 0]),
        // `pwritev2` needs its `flags`, which is the **sixth** argument.
        Syscall::Pwritev2 => to_glue(call, [a1, a2, a3, a4, a5, a6]),
        Syscall::Linkat => to_glue(call, [a1, a2, a3, a4, a5, 0]),
        Syscall::Fchownat => to_glue(call, [a1, a2, a3, a4, a5, 0]),
        Syscall::MemfdCreate => sys_memfd_create(a2),
        Syscall::Umask => to_glue(call, [a1, 0, 0, 0, 0, 0]),
        Syscall::Fsync | Syscall::Fdatasync => to_glue(call, [a1, 0, 0, 0, 0, 0]),
        // `sync(2)` takes no arguments; `syncfs(fd)` scopes the flush to the
        // filesystem holding the descriptor. Both were ENOSYS on this target
        // until 2026-10-05 — musl's `sync()` silently did nothing, and no
        // durability barrier existed anywhere below it (corruption doc §11).
        Syscall::Sync => to_glue(call, [0, 0, 0, 0, 0, 0]),
        Syscall::Syncfs => to_glue(call, [a1, 0, 0, 0, 0, 0]),
        // `posix_fadvise` — advisory by Linux's own contract, and akuma has
        // no readahead state to tune, so success is the honest answer rather
        // than a lie callers die on (parity-db `try_io!`s this). Same shape
        // as madvise's "unrecognised advice reports success", one level up.
        Syscall::Fadvise64 => 0,
        // busybox prints through `writev`, not `write`. Walk the iovec array and
        // forward each segment; a short write on any segment stops the walk, as
        // `writev(2)` specifies.
        Syscall::Writev => sys_writev(a1, a2, a3),
        Syscall::Readv => sys_readv(a1, a2, a3),
        Syscall::Openat => crate::fd::sys_openat(a1, a2, a3, a4),
        // `chdir(path)` / `fchmod(fd, mode)` / `fchmodat(dirfd, path, mode)` —
        // **glue's arms, with no preamble** (2026-09-12). All three have been
        // implemented in `akuma-syscalls-glue` since before this port existed;
        // the numbers just had no variants in `akuma-syscalls-abi`, so they
        // died as `ENOSYS` at the decode above — `git clone` on `chmod`,
        // busybox `top` on `chdir("/proc")`
        // (`docs/archive/RUST_TOOLCHAIN_AMD64.md`). Glue's `sys_chdir` reads
        // and writes `Process::cwd` — the per-process cwd this file's older
        // comments describe as absent — and every `*at` arm already resolves
        // `AT_FDCWD` against it through `resolve_path_at`, so a successful
        // `chdir` takes effect on the paths userspace opens next with no
        // further wiring. x86-only `chmod`(90) arrives as the `Fchmodat` shim
        // in the legacy block above.
        Syscall::Chdir => to_glue(call, [a1, 0, 0, 0, 0, 0]),
        // chroot(2) — glue's, so the root lives on the shared `Process`.
        Syscall::Chroot => to_glue(call, [a1, 0, 0, 0, 0, 0]),
        // `fchdir(fd)` — added 2026-09-20 alongside the `akuma-syscalls-abi`
        // row: the abi table alone was not enough, because this match has no
        // catch-all forwarding to glue (see `_ => errno::ENOSYS` below) — a
        // decodable `Syscall` still needs its own arm here or it falls into
        // that default. Same glue fn as `Chdir` reaches (`sys_fchdir`), just
        // keyed by fd instead of a path pointer.
        Syscall::Fchdir => to_glue(call, [a1, 0, 0, 0, 0, 0]),
        Syscall::Fchmod => to_glue(call, [a1, a2, 0, 0, 0, 0]),
        Syscall::Fchmodat => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        // `close(fd)` — **served by glue** (4b batch 2b). Two prerequisites had
        // to land before this arm could move, and neither was in the plan:
        // the two kernels had to share **one pipe table** (glue's `sys_close`
        // closes a `PipeRead`/`PipeWrite` through `glue::pipe`, and this
        // kernel's ids used to name a different table — a wrong pipe, not an
        // error), and the boot suite had to have a **process identity**, since
        // every descriptor-freeing arm in glue resolves
        // `current_process_shared()` first.
        Syscall::Close => {
            // Closing a `/dev/fb0` descriptor gives the screen back to the
            // console (`fbdev`). Any one of the owner's descriptors: POSIX
            // record locks follow the same rule, and a program holding two
            // `fb0` fds is not a case anything here produces.
            if crate::fd::is_dev_fb(a1)
                && let Some(p) = current_process()
            {
                crate::fbdev::release(p.tgid);
            }
            // The tap: host sockets and the gateway's slots go back (`crate::kvm::tun`).
            if crate::fd::is_dev_tun(a1) {
                crate::kvm::tun::release();
            }
            // A `/dev/wifi0` descriptor's read cursor (`crate::wifi`).
            if crate::fd::is_dev_wifi(a1)
                && let Some(p) = current_process()
            {
                crate::wifi::release(p.tgid, a1 as u32);
            }
            to_glue(call, [a1, 0, 0, 0, 0, 0])
        }
        // `lseek(fd, offset, whence)` — **glue's arm behind one preamble**
        // (4b batch 3a): a `/dev` character node, which Linux seeks to the
        // offset asked for and glue answers `0`/`ESPIPE` for. Not routed
        // through `to_glue` for that reason — see `fd::sys_lseek`.
        Syscall::Lseek => crate::fd::sys_lseek(a1, a2, a3),
        Syscall::Fstat => crate::fd::sys_fstat(a1, a2),
        Syscall::Ioctl => crate::fd::sys_ioctl(a1, a2, a3),
        // `getdents64(fd, dirp, count)` — x86_64 217. `ls`/`find`.
        // **Served by glue** (4b batch 3a) with no preamble at all: the record
        // layout was already `akuma_syscalls_linux::dirent` and the snapshot
        // was already `KernelFile::dir_cache`, so the two arms differed only
        // in what this one got wrong (`/dev` nodes listed as `DT_REG`, no
        // up-front validation of `dirp`).
        Syscall::Getdents64 => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        Syscall::Fcntl => crate::fd::sys_fcntl(a1, a2, a3),
        // `statfs(path, buf)` / `fstatfs(fd, buf)` — x86_64 137 / 138. `busybox
        // df` reads `/proc/mounts` and then calls `statfs` once per line; with
        // neither of them it printed a header and nothing else. Both report the
        // mount that actually serves the path, out of `fs.rs`'s mount table,
        // rather than one set of hardcoded numbers for the whole kernel.
        Syscall::Statfs => crate::fd::sys_statfs(a1, a2),
        Syscall::Fstatfs => crate::fd::sys_fstatfs(a1, a2),
        Syscall::Newfstatat => crate::fd::sys_newfstatat(a1, a2, a3, a4),
        // `statx` — arch-neutral struct, so this is glue's arm with no
        // preamble. New on this target (4b batch 3b); was `ENOSYS`.
        Syscall::Statx => crate::fd::sys_statx(a1, a2, a3, a4, a5),
        // `faccessat(dirfd, path, mode)` — glue's arm (4b batch 3c). The old
        // shim dropped `dirfd`; glue's `sys_faccessat2` honours it.
        Syscall::Faccessat => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        Syscall::Faccessat2 => to_glue(call, [a1, a2, a3, a4, 0, 0]),
        // `close_range(first, last, flags)`: glue frees the descriptors, but the three
        // amd64-only device descriptors (`/dev/fb0`, the tap, `/dev/wifi0`) are given back by
        // `Syscall::Close`'s preamble — so run those hooks for every open fd in the range first,
        // exactly as the per-fd fallback (`close` in a loop) did while this was `ENOSYS`.
        Syscall::CloseRange => {
            if a3 & 4 == 0
                && let Some(p) = current_process()
            {
                let (lo, hi) = (a1 as u32, a2 as u32);
                let fds: alloc::vec::Vec<u32> = akuma_primitives::irq::with_irqs_disabled(|| {
                    p.fds.table.lock().range(lo..=hi).map(|(&fd, _)| fd).collect()
                });
                for fd in fds {
                    let f = u64::from(fd);
                    if crate::fd::is_dev_fb(f) {
                        crate::fbdev::release(p.tgid);
                    }
                    if crate::fd::is_dev_tun(f) {
                        crate::kvm::tun::release();
                    }
                    if crate::fd::is_dev_wifi(f) {
                        crate::wifi::release(p.tgid, fd);
                    }
                }
            }
            to_glue(call, [a1, a2, a3, 0, 0, 0])
        }
        // `dup(fd)` — x86_64 32. `apk` dups a reopened index fd during
        // signature-verification I/O setup; `ENOSYS` here made it report
        // `UNTRUSTED signature` over a fetch that was fine (see
        // `fd::sys_dup`; the aarch64 twin is `APK_MISSING_SYSCALLS.md`).
        Syscall::Dup => crate::fd::sys_dup(a1),
        Syscall::Dup3 => crate::fd::sys_dup3(a1, a2, a3),
        Syscall::Pipe2 => crate::fd::sys_pipe2(a1, a2),
        // `mkdirat` (258) / `unlinkat` (263) / `renameat` (264) — **served by
        // glue** (4b step 2, batch 1: the path-only `*at` family). `apk`'s
        // cache write is a named `.tmp.<pid>` file plus a rename; without
        // these the cache write fails and the index fetch is unusable (the
        // aarch64 table in `APK_MISSING_SYSCALLS.md` lists all three). The
        // VFS underneath is the same `akuma_vfs_glue` mount walk both kernels
        // share, so the arm answers identically; the dirfd resolution and
        // errno-table divergences the fold adopted are stated in
        // `AKUMA_AMD64_4B_FOLD_BATCH1.md`.
        Syscall::Mkdirat => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        Syscall::Unlinkat => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        Syscall::Renameat => to_glue(call, [a1, a2, a3, a4, 0, 0]),
        // `mount(source, target, fstype, flags, data)` / `umount2(target,
        // flags)` — glue's arms (`sc-containers`), same mount table this
        // target's own root and every `*at` call above already resolve
        // through. `caller_may_mount` there is "box 0 only", which every
        // process on this target satisfies (`box_id: 0`, hardcoded — see
        // `net.rs`), so this is real mount/unmount rather than a stub.
        //
        // The box-lifecycle calls the same feature also compiles
        // (`register_box`/`kill_box`/`reattach`/`mount_in_ns`) are Akuma-private
        // numbers and are dispatched through the `AKUMA_PRIVATE_BASE` match
        // above (315-318, 324, 325), not here. See
        // `docs/archive/AKUMA_AMD64_BOXES_AUDIT.md`.
        Syscall::Mount => to_glue(call, [a1, a2, a3, a4, a5, 0]),
        Syscall::Umount2 => to_glue(call, [a1, a2, 0, 0, 0, 0]),
        // `readlinkat` / `symlinkat` — **served by glue** (4b step 2, batch 1).
        // Glue's readlinkat is strictly more than the arm it replaces: it
        // distinguishes `EINVAL` (path exists, not a symlink) from `ENOENT`
        // where the local arm collapsed both to `ENOENT`, serves
        // `/proc/self/exe` from the registered image, and describes
        // non-file fds under `/proc/<pid>/fd`.
        Syscall::Readlinkat => to_glue(call, [a1, a2, a3, a4, 0, 0]),
        // `symlinkat(target, newdirfd, link_path)` — x86_64 266. Package
        // contents are full of `.so.1` versioned-library symlinks; ENOSYS
        // here turned each into a counted `apk add` error.
        Syscall::Symlinkat => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        // `utimensat(dirfd, path, times, flags)` — glue's arm (4b batch 3c).
        // Timestamp preservation for `apk add`'s post-extract pass. `struct
        // timespec` is LP64 on both architectures and `UTIME_NOW`/`UTIME_OMIT`
        // plus the `AT_*` flags are shared, so no translation. Glue's arm adds
        // the `path == NULL` (`futimens(fd)`) form and validates the ns range
        // ahead of any lookup. There is no `futimens` syscall to pair with —
        // libc spells it `utimensat(fd, NULL, times, 0)` — which is what the
        // x86_64 88 arm above used to be mistaken for.
        Syscall::Utimensat => to_glue(call, [a1, a2, a3, a4, 0, 0]),
        // `getcwd(buf, size)` — **glue's arm** (2026-09-12). The local version
        // here predated per-process cwd and hard-coded `/`; since `Chdir`
        // landed above, a `getcwd` that ignored `Process::cwd` would report a
        // directory the process is not in. Glue returns the length *including*
        // the NUL, same as the arm it replaces.
        Syscall::Getcwd => to_glue(call, [a1, a2, 0, 0, 0, 0]),
        // `flock(fd, op)` — x86_64 73. One user, one process at a time on
        // this target (no fork-based package-manager concurrency exists to
        // race against), so there is nothing an advisory lock could actually
        // protect — accept and do nothing, the same stance `mprotect` above
        // takes. Without this, `apk`'s database lock (`flock` on
        // `/lib/apk/db/lock`) came back `ENOSYS` and it treated that as fatal:
        // `apk update` printed "Unable to lock database: Function not
        // implemented" and exited before ever reaching the network.
        Syscall::Flock => 0,
        // `io_setup`/`io_destroy`/`io_submit`/`io_cancel`/`io_getevents` —
        // Linux native AIO, glue's arms (`sc-aio`). `io_setup`'s `ctx_idp` is
        // a VA the caller's `libc` dereferences directly (the mmap'd
        // `aio_ring`), so it goes through glue's own `read_user_into`/
        // `write_user_val` exactly as on AArch64 — nothing here decodes it.
        Syscall::IoSetup => to_glue(call, [a1, a2, 0, 0, 0, 0]),
        Syscall::IoDestroy => to_glue(call, [a1, 0, 0, 0, 0, 0]),
        Syscall::IoSubmit => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        Syscall::IoCancel => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        Syscall::IoGetevents => to_glue(call, [a1, a2, a3, a4, a5, 0]),
        _ => return None,
    })
}

/// Readiness and event descriptors: `ppoll`, `pselect6`, epoll, `eventfd`, `timerfd`.
#[allow(clippy::inline_always)] // one switch, not eight calls: see the module doc
#[inline(always)]
fn poll(call: Syscall, a: [u64; 6]) -> Option<u64> {
    let [a1, a2, a3, a4, a5, a6] = a;
    Some(match call {
        // `ppoll(fds, nfds, *timespec, sigmask, sigsetsize)` — x86_64 271, and
        // `pselect6(nfds, r, w, e, *timespec, *sigmask)` — x86_64 270. Both are
        // **glue's arms** since 4b batch 4b, and both read their own
        // `struct timespec` there (`akuma_syscalls_time::read_timeout_us`, the
        // one decoder in the tree). This arm used to fold the timespec down to
        // milliseconds here, which silently floored every sub-millisecond
        // `ppoll` to zero.
        //
        // `pselect6` is dispatched for completeness, not because anything issues
        // it: x86_64 musl's `select()` compiles its `#ifdef SYS_select` branch
        // and sends **23** (`fd::sys_select`), where aarch64 musl has no
        // `select` number at all and sends 72. A program calling
        // `syscall(SYS_pselect6, …)` by hand used to get `ENOSYS`.
        Syscall::Ppoll => crate::fd::sys_ppoll(a1, a2, a3, a4),
        Syscall::Pselect6 => to_glue(call, [a1, a2, a3, a4, a5, a6]),
        // `epoll_create1(flags)` / `epoll_ctl(epfd, op, fd, *event)` /
        // `epoll_pwait(epfd, events, maxevents, timeout, sigmask, sigsetsize)`
        // — all three are glue's arms (`sc-epoll`), sharing the same
        // `akuma_net_yarn::WaitPolicy` blocking-wait machine `Pselect6` above
        // already exercises on this target, so the park/wake path needs no
        // amd64-specific arm the way `Ppoll`'s does.
        Syscall::EpollCreate1 => to_glue(call, [a1, 0, 0, 0, 0, 0]),
        Syscall::EpollCtl => to_glue(call, [a1, a2, a3, a4, 0, 0]),
        Syscall::EpollPwait => to_glue(call, [a1, a2, a3, a4, a5, a6]),
        // `eventfd2(initval, flags)` — glue's arm (`sc-eventfd`). The
        // cross-thread wakeup a `mio` epoll reactor uses to interrupt a
        // blocking `epoll_pwait`.
        Syscall::Eventfd2 => to_glue(call, [a1, a2, 0, 0, 0, 0]),
        // `timerfd_create(clockid, flags)` / `timerfd_settime(fd, flags,
        // new_value, old_value)` / `timerfd_gettime(fd, out)` — glue's arms
        // (`sc-timerfd`). `struct itimerspec` needs no boundary translation
        // (see the row's own doc comment in `akuma-syscalls-abi`), so unlike
        // epoll these are a straight forward with no ABI fix alongside them.
        Syscall::TimerfdCreate => to_glue(call, [a1, a2, 0, 0, 0, 0]),
        Syscall::TimerfdSettime => to_glue(call, [a1, a2, a3, a4, 0, 0]),
        Syscall::TimerfdGettime => to_glue(call, [a1, a2, 0, 0, 0, 0]),
        _ => return None,
    })
}

/// The address space: `mmap` and its relatives, `brk`, `membarrier`.
#[allow(clippy::inline_always)] // one switch, not eight calls: see the module doc
#[inline(always)]
fn mem(call: Syscall, a: [u64; 6]) -> Option<u64> {
    let [a1, a2, a3, a4, a5, a6] = a;
    Some(match call {
        Syscall::Membarrier => to_glue(call, [a1, 0, 0, 0, 0, 0]),
        // `a5` is mmap's fd and is deliberately unused: only anonymous mappings
        // are supported, so a file-backed request must fail rather than quietly
        // return zeroed memory that the caller believes holds a file.
        // `a6` is the file offset. It was dropped until 2026-09-07 because only
        // anonymous mappings were served and an anonymous `mmap`'s offset is
        // ignored; a file mapping cannot ignore it.
        // Census stub (`kvm::api`): a `/dev/kvm` descriptor maps as anonymous memory, enough to see
        // what the VMM does with the vCPU's run page.
        Syscall::Mmap if let Some(id) = crate::fd::dev_kvm_api_id(a5) => {
            crate::kvm::api::note_mmap(a5, a1, a2, a3, a4, a6);
            // A vCPU descriptor maps the run page; the others (nothing maps them) get anonymous memory.
            let r = crate::kvm::api::mmap_vcpu(id, a1, a2, a3, a4);
            if r == crate::fd::errno::ENODEV { crate::mm::sys_mmap(a1, a2, a3, a4 | 0x20, u64::MAX, 0) } else { r }
        }
        Syscall::Mmap => crate::mm::sys_mmap(a1, a2, a3, a4, a5, a6),
        Syscall::Munmap => crate::mm::sys_munmap(a1, a2),
        // `madvise(addr, len, advice)`. Answered `ENOSYS` for everything until
        // 2026-09-07, which is the *wrong* way to say "not implemented": Linux
        // says `EINVAL` for unsupported advice and redis exits on anything else.
        Syscall::Madvise => crate::mm::sys_madvise(a1, a2, a3),
        // `mremap(old, old_len, new_len, flags)`. `a5` is `MREMAP_FIXED`'s
        // `new_address` and is deliberately not passed: this target does not
        // support `MREMAP_FIXED`, and the decision crate does not decode it.
        Syscall::Mremap => crate::mm::sys_mremap(a1, a2, a3, a4),
        // `mprotect` — real since the region table landed (2026-09-07). It
        // splits the regions the range crosses and re-permissions the pages
        // that are present; see `mm::sys_mprotect` for why it was `return 0`
        // for so long and what that cost.
        Syscall::Mprotect => crate::mm::sys_mprotect(a1, a2, a3),
        // `brk(addr)` — x86_64 12. This target has no program break (the
        // allocators are `mmap`-based and the arm was removed), so the row
        // decoded and fell into the silent default below: `ENOSYS`, an answer
        // Linux's `brk` never gives. Linux's answer for a break that cannot
        // move is the **current break, unchanged**; musl's `__expand_heap`
        // reads that as "no growth" and falls back to `mmap`, which is what it
        // did here anyway — now through the documented path. Nothing is mapped.
        // Found 2026-10-08 by the `[sc!]` trace: 54 `brk -> -ENOSYS` lines in
        // one Chromium run.
        Syscall::Brk => current_process().map_or(0, |p| p.get_brk() as u64),
        // `Msync` never gets here: 26 is routed above, before the decode.
        Syscall::Msync => errno::ENOSYS,
        _ => return None,
    })
}

/// Sockets. A guarded arm sends `AF_UNIX` to glue; the unguarded one after it
/// is this kernel's TCP/IP.
#[allow(clippy::inline_always)] // one switch, not eight calls: see the module doc
#[inline(always)]
fn net(call: Syscall, a: [u64; 6]) -> Option<u64> {
    let [a1, a2, a3, a4, a5, a6] = a;
    Some(match call {
        // `socketpair(domain, type, protocol, sv)` — glue's AF_UNIX pair, two
        // kernel pipes behind two `FileDescriptor::UnixSocket` entries. It
        // needs no arm of its own here beyond the hop: the descriptors land in
        // `Process::fds`, which is the table `crate::fd` allocates into, and
        // `read`/`write`/`close` on this target are already glue's.
        Syscall::Socketpair => to_glue(call, [a1, a2, a3, a4, 0, 0]),
        // **AF_UNIX first, then the native stack** — same rule as `Sendto`/
        // `Recvfrom` below. `socket(AF_UNIX, …)` used to answer `EAFNOSUPPORT`
        // here (nca's IPC listener, `os error 97`); glue's `unixsock` has always
        // been compiled in and serves `bind`/`listen`/`accept`/`connect` for the
        // descriptors it hands out, so the family is decided by the domain at
        // `socket` and by the descriptor everywhere after.
        Syscall::Socket if a1 == AF_UNIX => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        Syscall::Socket => crate::sock::sys_socket(a1, a2, a3),
        Syscall::Bind if crate::fd::is_unix_socket(a1) => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        Syscall::Bind => crate::sock::sys_bind(a1, a2, a3),
        Syscall::Listen if crate::fd::is_unix_socket(a1) => to_glue(call, [a1, a2, 0, 0, 0, 0]),
        Syscall::Listen => crate::sock::sys_listen(a1, a2),
        Syscall::Accept if crate::fd::is_unix_socket(a1) => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        Syscall::Accept => crate::sock::sys_accept(a1, a2, a3),
        Syscall::Accept4 if crate::fd::is_unix_socket(a1) => to_glue(call, [a1, a2, a3, a4, 0, 0]),
        Syscall::Accept4 => crate::sock::sys_accept4(a1, a2, a3, a4),
        Syscall::Connect if crate::fd::is_unix_socket(a1) => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        Syscall::Connect => crate::sock::sys_connect(a1, a2, a3),
        // `a5` is the destination/source `struct sockaddr *` — Linux's 5th
        // argument, which the entry asm's shuffle keeps (only the 6th,
        // `addrlen`, is dropped; see `syscall_entry`'s comment on why that was
        // safe until now). Needed for UDP: unlike a connected TCP socket, a
        // UDP `sendto` has no peer to fall back on, and musl's DNS resolver
        // never `connect()`s its query socket — it addresses every nameserver
        // by hand on each `sendto`. See `sock::sys_sendto`.
        // **AF_UNIX first, then the native stack** — the shape glue's own
        // dispatchers have, and the reason these two arms are guarded rather
        // than left to `crate::sock`. A `socketpair(2)` endpoint is not a
        // `FileDescriptor::Socket`, so `sock::sys_recvfrom` answers `ENOTSOCK`
        // for a descriptor the kernel handed out three syscalls earlier — which
        // is what Rust `std` hit the moment `socketpair` started working
        // (`the CLOEXEC pipe failed: Not a socket`). Its spawn channel is a
        // `SOCK_SEQPACKET` pair, so it `recv`s rather than `read`s, and `recv`
        // on this target had no AF_UNIX arm at all.
        //
        // Glue's `sys_sendto`/`sys_recvfrom` test the family themselves and
        // serve AF_UNIX from `unixsock`; the hop is here so `crate::sock` stays
        // the smoltcp implementation rather than growing a second family.
        Syscall::Sendto if crate::fd::is_unix_socket(a1) => {
            to_glue(call, [a1, a2, a3, a4, a5, a6])
        }
        Syscall::Sendto => crate::sock::sys_sendto(a1, a2, a3, a5),
        // Both families, then the busy-loop tripwire: the 2026-09-25 kot spin
        // was ~720k `recvfrom`/s from one thread and no log named the socket.
        Syscall::Recvfrom => {
            let unix = crate::fd::is_unix_socket(a1);
            let r = if unix {
                to_glue(call, [a1, a2, a3, a4, a5, a6])
            } else {
                crate::sock::sys_recvfrom(a1, a2, a3, a5)
            };
            crate::sock::recv_spin_tripwire(a1, a3, a4, r, unix);
            r
        }
        // AF_UNIX first, as for `Sendmsg`: `crate::sock` answered `ENOTSOCK`
        // for a unix fd, and Chromium's crashpad `CHECK`-crashes the browser
        // on `setsockopt(SO_PASSCRED)` failing (2026-10-08). Glue accepts the
        // `SOL_SOCKET` options on a unix socket, as it always has on AArch64.
        Syscall::Setsockopt if crate::fd::is_unix_socket(a1) => to_glue(call, [a1, a2, a3, a4, a5, 0]),
        Syscall::Setsockopt => crate::sock::sys_setsockopt(a1, a2, a3, a4, a5),
        // `getsockopt`/`shutdown` never had a native `crate::sock` arm at all —
        // unlike `Setsockopt`/`Sendto`, there is no "second family" to avoid
        // growing by going through glue; `net::dispatch_getsockopt`/
        // `dispatch_shutdown` resolve the same `FileDescriptor::Socket(idx)`
        // into the same `akuma_net::socket` table `sys_socket` above allocates
        // out of. Without `Shutdown`, a program doing the ordinary
        // half-close-then-drain pattern (`shutdown(fd, SHUT_WR)`, keep reading
        // until EOF) got `ENOSYS` on the shutdown and never sent the FIN.
        // Without `Getsockopt`, `SO_ERROR` after a non-blocking `connect()`
        // that a poller reported writable was unreadable, so the caller could
        // not tell success from a refused connection.
        Syscall::Getsockopt => to_glue(call, [a1, a2, a3, a4, a5, 0]),
        Syscall::Shutdown => to_glue(call, [a1, a2, 0, 0, 0, 0]),
        // `getsockname`/`getpeername` answered **`ENOSYS`** here until
        // 2026-09-18 — no arm at all, native or glue. Glue has implemented both
        // for as long as AArch64 has had sockets (`net::dispatch_getsockname`,
        // which also serves AF_UNIX), so this is a missing hop rather than
        // missing code, exactly like `Getsockopt` above.
        //
        // Found by a probe replaying c-ares' resolver sequence: it is one of
        // three things `curl`/`git` needed on a *connected* UDP socket that
        // this target did not answer. An `ENOSYS` from a syscall a library
        // treats as infallible is worse than an error — it reads as "this
        // kernel is broken" rather than "that failed", which is the same trap
        // `AKUMA_AMD64_MEMORY_CLOSEOUT.md` §"the errno, not the feature"
        // records for `madvise`.
        Syscall::Getsockname | Syscall::Getpeername => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        // `sendmsg`/`recvmsg` — x86_64 46/47. musl's DNS resolver on this
        // build uses these, not `sendto`/`recvfrom`; without them `poll`
        // correctly reported a UDP reply readable and `recvmsg` came back
        // `ENOSYS`, so `apk`'s own name resolution spun forever. See
        // `sock::sys_sendmsg`/`sock::sys_recvmsg`.
        //
        // **AF_UNIX first**, same rule as `Socket`/`Sendto` above: a unix fd
        // is not a `FileDescriptor::Socket`, so `crate::sock` answered it
        // `ENOTSOCK` — plain unix `sendmsg` did not work on this target, and
        // glue's is where `SCM_RIGHTS` lives (Chromium's IPC passes 600+ fds
        // loading one page).
        Syscall::Sendmsg if crate::fd::is_unix_socket(a1) => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        Syscall::Recvmsg if crate::fd::is_unix_socket(a1) => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        Syscall::Sendmsg => crate::sock::sys_sendmsg(a1, a2, a3),
        Syscall::Recvmsg => crate::sock::sys_recvmsg(a1, a2, a3),
        _ => return None,
    })
}

/// Process lifecycle, identity, credentials, sessions, scheduling and `futex`.
#[allow(clippy::inline_always)] // one switch, not eight calls: see the module doc
#[inline(always)]
fn process(call: Syscall, a: [u64; 6]) -> Option<u64> {
    let [a1, a2, a3, a4, a5, a6] = a;
    Some(match call {
        Syscall::Prctl => to_glue(call, [a1, a2, a3, a4, a5, 0]),
        // `rustc` sizes its codegen thread pool from this one, so an `ENOSYS`
        // was quietly making every in-guest build single-threaded.
        Syscall::SchedGetaffinity => sys_sched_getaffinity(a1, a2, a3),
        Syscall::SchedSetaffinity => sys_sched_setaffinity(a1, a2, a3),
        // `exit` (60) and `exit_group` (231) are the same call for a
        // single-threaded process and emphatically not for a threaded one:
        // `exit` ends the calling thread, `exit_group` ends every thread in the
        // group. musl's `pthread_exit` uses the first and `main` returning uses
        // the second, so conflating them makes the first thread to finish take
        // the whole process with it.
        Syscall::ExitGroup => {
            // From a thread that is not the leader, the leader must be told as
            // well — see `thread::exit_group_from_thread`.
            if !crate::thread::current_is_main()
                && let Some(p) = akuma_exec::process::current_process_shared()
            {
                crate::thread::exit_group_from_thread(current_proc_slot(), p.tgid, a1);
            }
            exit_current_group();
            a1
        }
        Syscall::Exit => {
            // A main thread's `exit` is a process exit: nothing else runs in
            // its address space, and `run_process` is the only frame below it.
            // A non-main thread's is not — it must leave ring 3 without
            // touching the fd table or the spawn record, both of which belong
            // to the process it is only one thread of. Either way the status
            // is `a1`, carried out in `%rax`.
            mark_leave();
            a1
        }
        Syscall::Getpriority | Syscall::Setpriority => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        // `getpid`/`getppid` — **served by glue** (2026-09-11). Both answered a
        // literal `1`, which was true once and stopped being true when 5b slice 2
        // gave this target a real process table: `usermode::current_pid()` has
        // resolved the caller's tgid since then, and only these two arms had not
        // been told.
        //
        // A constant `1` is not a small lie here. `sys_kill` refuses `pid <= 1`
        // with `EPERM`, so `kill(getpid(), sig)` — a spelling of `raise` that
        // real programs use — could never work. `$$` was 1 in every shell on the
        // machine at once (measured on the metal), so anything naming a temp
        // file, a lock or a log line after its own pid collided with everything
        // else, and no `ps` output could be correlated with it.
        //
        // **Not** `raise(3)` or `pthread_kill(3)`, and the difference is worth
        // stating because it is the plausible-sounding wrong answer: musl spells
        // both with `tkill`, which takes a *thread* id and never consults this.
        // glibc spells them with `tgkill(getpid(), …)` and would have been
        // broken by the constant — which matters the day something
        // dynamically-linked and non-musl runs here, and not before.
        Syscall::Getpid | Syscall::Getppid => to_glue(call, [a1, a2, a3, a4, a5, a6]),
        // The child-tid futex address a threaded libc registers on startup.
        // Single-address-space, no `CLONE_THREAD` here, so it is recorded
        // nowhere and the return value (the caller's tid) is ignored.
        // `set_tid_address(tidptr)` — **served by glue** (2026-09-11). It was a
        // literal `1`, and that constant is what made `raise(3)` a no-op on this
        // target: musl's `__init_tp` seeds `pthread_self()->tid` from this
        // syscall's return, and `raise` is `tkill(pthread_self()->tid, sig)`. So
        // every self-signal in every program here addressed **thread slot 1**,
        // which is not the caller. Glue returns `current_thread_id()` and records
        // `clear_child_tid` on the way, which is the other half this arm dropped.
        Syscall::SetTidAddress => to_glue(call, [a1, a2, a3, a4, a5, a6]),
        Syscall::SchedYield => {
            // The switch happens on *this task's* kernel stack, which is the
            // whole reason UserCtx is per-task: two processes sharing one
            // syscall stack would clobber each other's saved frame here.
            crate::sched::yield_now();
            0
        }
        // `execve(path, argv, envp)` — x86_64 59: the current (spawned or
        // forked) task replaces its own image in place. See `sys_execve`.
        Syscall::Execve => sys_execve(a1, a2, a3),
        Syscall::Clone => {
            // `CLONE_VM` is the fork/thread fork in the road, and the only one:
            // with it the caller wants to share an address space
            // (`crate::thread`), without it it wants a copy (`sys_fork`).
            //
            // x86_64's argument order is its own: `(flags, child_stack,
            // parent_tid, child_tid, tls)` — `tls` **last**, after `child_tid`,
            // where most architectures put it fourth.
            // The decision is `akuma_syscalls_proc::clone::decode`, shared with
            // Taoxichuan; it also answers Linux's `EINVAL` for `CLONE_THREAD`
            // without `CLONE_SIGHAND`/`CLONE_VM` (2026-11-01).
            use akuma_syscalls_proc::clone::{self as clone_decode, Kind};
            let kind = match clone_decode::decode(a1, a2) {
                Ok(k) => k,
                Err(clone_decode::Invalid) => return Some(errno::EINVAL),
            };
            if kind == Kind::Thread {
                // `CLONE_VM|CLONE_THREAD` — a thread (`crate::thread`).
                return Some(crate::thread::sys_clone_thread(a1, a2, a3, a4, a5));
            }
            if let Kind::SpawnFork { child_stack } = kind {
                // **`CLONE_VM` without `CLONE_THREAD` — musl's `posix_spawn`
                // child** (`clone(CLONE_VM|CLONE_VFORK|SIGCHLD)`, a separate
                // stack, no `CLONE_SETTLS`). It was `ENOSYS` here for the whole
                // life of the port, which is the one arm every compile driver
                // dies on: gcc, clang and rustc all run their subprograms
                // through `posix_spawn` (`docs/archive/RUST_TOOLCHAIN_AMD64.md`).
                //
                // Served as a **fork whose child enters on the clone-supplied
                // stack** — `fork_process`'s `stack_ptr` parameter, which sets
                // the child's `rsp` and leaves its syscall return `0`. That is
                // exactly the entry convention musl's `__clone` arranges:
                // `[rsp]` is the child function, `[rsp+8]` its argument, so the
                // child pops both and calls `fn(arg)` — which is `execve` or
                // `_exit`, never a return into shared code. Two deliberate
                // divergences from Linux, both safe here:
                //
                // * the address space is **copied (CoW), not shared**. `vfork`
                //   sharing is only sound with the parent suspended until exec
                //   or exit, and this target does not suspend; a copy makes the
                //   unsuspendable race uninteresting, at the cost of one CoW
                //   share per spawn — the thing `vfork` exists to avoid, paid
                //   where it is correctness rather than speed that is for sale.
                // * the parent is **not** suspended (`CLONE_VFORK` ignored).
                //   musl's spawn parent only `waitpid`s, so it never observes
                //   the difference.
                //
                // `CLONE_SETTLS` is *absent* on this path by construction — the
                // spawn child inherits the parent's `%fs` — which is correct:
                // it execs immediately, and `execve` installs the new image's
                // TLS. CoW fork runs at `SMP>1` (the shootdown, 2026-09-09) and
                // from a multithreaded parent (the share pass and the break
                // serialize on the leader's address-space lock). Go's
                // `os/exec` was dying through this path for a month and the
                // cause was none of that: each child's exit is a `SIGCHLD`,
                // and a tick-time signal used to return through `sysretq`
                // with `RCX = RIP` (`docs/archive/AMD64_FORK_VS_THREADS_SEGV.md`).
                return Some(sys_spawn_clone(child_stack));
            }
            sys_fork()
        }
        // `gettid` — x86_64 186. Its own tid for a thread, its pid for a main
        // thread. Rust's `std` prints it in a panic message, which is how its
        // absence announced itself: `thread 'main' (18446744073709551615)`.
        // `gettid` — **served by glue** (2026-09-11), for the same reason and
        // with the same consequence as `set_tid_address` above. This answered
        // `crate::thread::current_tid()`, which for a process's *main* thread
        // falls back to the **pid** rather than the thread slot — so `gettid()`
        // and the tid `clone` hands a child came from two different namespaces
        // on one target, and `tkill(gettid(), …)` addressed whatever task
        // happened to occupy slot `pid`. The slot is the namespace every
        // per-thread array in `akuma-threading` is indexed by (pending signals,
        // masks, sigaltstacks, wakers), which `clone_thread`'s own comment says
        // at length.
        //
        // This arm was `current_tid`'s **only** caller, so the fold deleted the
        // function and the `Thread::tid` field behind it — which held
        // `task as u32`, the slot, i.e. exactly what glue answers. The two
        // namespaces met only in the main-thread fallback, and that is where the
        // divergence was.
        Syscall::Gettid => to_glue(call, [a1, a2, a3, a4, a5, a6]),
        // `futex` — x86_64 202. Six arguments, which is why `syscall_entry`
        // now forwards `a6`.
        Syscall::Futex => crate::futex::sys_futex(a1, a2, a3, a4, a5, a6),
        // `wait4(pid, wstatus, options, rusage)` — x86_64 61. **Served by
        // `akuma-syscalls-glue` since 2026-09-10**, which is the whole point of
        // this target registering an exit `ProcessChannel` per child
        // (`register_exec_process`).
        //
        // What it replaced was a hand-written loop over this file's own
        // `sys_waitpid`, parking on a global bitmap of waiters that any child's
        // exit woke wholesale. It worked, and it could not answer three things
        // the shared implementation answers for free:
        //
        // * **`ECHILD` for a non-child.** `is_child_of_group` resolves the
        //   recorded parent's *thread group*, so a multithreaded parent waiting
        //   from a non-leader thread gets the right answer — and Go's pidfd
        //   probe, which requires `ECHILD` from `waitid(P_PIDFD)` on itself,
        //   stops deadlocking against its own exit.
        // * **`EINTR`.** The loop here had no interrupt check at all; a `wait4`
        //   was uninterruptible.
        // * **`rusage`.** Zeroed rather than ignored, which is what a caller
        //   that passes a pointer expects to find.
        //
        // The reap moved with it: glue unregisters the process and reaps the
        // child channel, so this target's `SPAWN` row is collected by
        // [`sweep_reaped_spawn_rows`] rather than freed inline. See its doc for
        // why a sweep and not a hook.
        Syscall::Wait4 => {
            let r = to_glue(Syscall::Wait4, [a1, a2, a3, a4, 0, 0]);
            sweep_reaped_spawn_rows();
            r
        }
        // Credentials — **served by glue** (C1 step 3, second batch).
        //
        // Chosen next because they are the whole of `akuma_syscalls::fast_path`'s
        // `Leaf` tier that this target dispatches: they take no arguments and
        // consult no `Process`, so glue's prologue skips the identity resolve
        // and there is nothing here for a process table this target does not
        // populate to answer wrongly. Both kernels already returned a literal
        // `0` (glue's `geteuid` is a function whose entire body is `0`), so this
        // is the rare fold with no divergence to pin: the answer is identical
        // and only the number of places it is written down changes.
        //
        // **Not folded, deliberately:** `getpid`/`gettid`/`getppid`/`getpgid`/
        // `getsid`/`getcwd` below. Those *are* identity — glue reads them out of
        // `akuma_exec`'s process table, which this target does not fill, so it
        // would answer confidently and wrongly rather than not at all. They wait
        // for C1 step 5, the same step `exec_runtime.rs`'s `futex_wake` stub
        // names. See `docs/archive/AKUMA_AMD64_C1_STEP3_PREREQUISITES.md`.
        Syscall::Getuid
        | Syscall::Getgid
        | Syscall::Geteuid
        | Syscall::Getegid
        // `setuid`/`setgid`: already root, accept. Glue folds them in with
        // `capset`/`setres[ug]id`/`setgroups` under one stated stance —
        // "success" here means *not implemented*, not "privileges dropped".
        | Syscall::Setuid
        | Syscall::Setgid
        // `capget(hdr, data)` / `capset(hdr, data)` — x86_64 125/126, rows
        // added 2026-10-08. Chromium's zygote child calls `capset` right after
        // `fork`, inside a `CHECK`, before it pings the browser; `ENOSYS` here
        // killed every child (`[signal] pid=61 killed by signal 6`) and the
        // browser gave up on the GPU process. Glue's `capset` is the same
        // accepted no-op as `setuid` above, and its `capget` does the version
        // negotiation and reports root's full set. Gate: `capprobe.c`.
        | Syscall::Capget
        | Syscall::Capset
        // `getgroups(size, list)` — x86_64 115. Not a fold: this target had no
        // arm for it at all, and glue's has been there all along. Found by the
        // ring-3 check this batch was verified with — `busybox id` on the metal
        // printed `uid=0 gid=0` (the folded arms answering) and then
        // `id: can't get groups` and exited 1. No supplementary groups exist
        // here, so the answer is the count `0`, and `size == 0` is the probe
        // form every caller actually uses.
        | Syscall::Getgroups
        // The rest of the credential family (2026-10-14). Real now on both
        // kernels: `akuma_isolation::creds`, through glue's `cred.rs`.
        | Syscall::Setreuid
        | Syscall::Setregid
        | Syscall::Setgroups
        | Syscall::Setresuid
        | Syscall::Getresuid
        | Syscall::Setresgid
        | Syscall::Getresgid
        | Syscall::Setfsuid
        | Syscall::Setfsgid
        | Syscall::Fchown => to_glue(call, [a1, a2, a3, a4, a5, a6]),
        // Best-effort robustness/rlimit hooks musl pokes on startup.
        Syscall::SetRobustList => 0,          // set_robust_list
        // `prlimit64(pid, resource, new, old)` — **served by glue** (C1 step 3,
        // batch 3). A leaf: it reads a fixed table and `ExecConfig`, never a
        // `Process`.
        //
        // This arm was `=> 0`, which is not a stub but a **wrong answer**:
        // `prlimit64` returning success without writing `old_rlim` leaves the
        // caller reading whatever was on its stack as its own stack and
        // file-descriptor limits. musl's `getrlimit` is this syscall, and a
        // build system asking `RLIMIT_NOFILE` before sizing a poll set is
        // exactly the shape that then fails somewhere else entirely.
        // Glue fills it: `RLIMIT_STACK` from `ExecConfig::user_stack_size`
        // (real on this target — `exec_runtime.rs`), `RLIMIT_NOFILE` 1024,
        // everything else `RLIM_INFINITY`.
        Syscall::Prlimit64 => to_glue(call, [a1, a2, a3, a4, a5, a6]),
        // Process-group / session ids. One process, so it is its own group and
        // session leader; `setpgid`/`setsid` accept and report id 1.
        // `getpgid`/`getsid` still answer `1` — **deliberately, and the reason is
        // the console**. `TerminalState::foreground_pgid` defaults to 1 and
        // `kill_foreground_group` excludes the group leader, which is what makes
        // `^C` reach `init`'s children and not `init` (`crate::console`). A shell
        // that read a real `getpgid` and then `TIOCSPGRP`'d it would move that
        // target, and nothing here has been tested against job control. Fold
        // them when something needs them, with the Ctrl-C gate in hand.
        //
        // **Something needed them: a pty session** (2026-10-05). A shell on a
        // pseudo-terminal does job control against *that* terminal's foreground
        // group — busybox `ash` loops `killpg(0, SIGTTIN)` until
        // `tcgetpgrp() == getpgrp()` — so for a process whose session a pty
        // controls the answers are real, and for everything else (the console
        // world this comment is about) they are exactly what they were.
        // `setsid` is real everywhere: it is how a pty child gets a group of
        // its own before `TIOCSCTTY`, and it detaches from any terminal, which
        // touches nothing the console reads. `akuma_syscalls_glue::pty`.
        Syscall::Getpgid => akuma_syscalls_glue::pty::sys_getpgid_gated(a1 as u32),
        Syscall::Getsid => akuma_syscalls_glue::pty::sys_getsid_gated(a1 as u32),
        Syscall::Setpgid => akuma_syscalls_glue::pty::sys_setpgid_gated(a1 as u32, a2 as u32),
        Syscall::Setsid => akuma_syscalls_glue::pty::sys_setsid(),
        Syscall::Getrusage => to_glue(call, [a1, a2, 0, 0, 0, 0]),
        // `waitid(idtype, id, infop, options, rusage)` — x86_64 247, glue's (`sys_waitid`: `P_PID`/`P_ALL`/
        // `P_PIDFD`, `WNOHANG`/`WNOWAIT`, the same group-wise child lookup and `ECHILD` rules as `wait4`).
        // Answered `ENOSYS` until 2026-11-09, which Bun and Go's pidfd probe both ask. Like `wait4` it reaps
        // (unless `WNOWAIT`), so the `SPAWN` row sweep follows.
        Syscall::Waitid => {
            let r = to_glue(Syscall::Waitid, [a1, a2, a3, a4, a5, 0]);
            sweep_reaped_spawn_rows();
            r
        }
        _ => return None,
    })
}

/// Signal disposition, masks, delivery and return.
#[allow(clippy::inline_always)] // one switch, not eight calls: see the module doc
#[inline(always)]
fn signal(call: Syscall, a: [u64; 6]) -> Option<u64> {
    let [a1, a2, a3, a4, a5, a6] = a;
    Some(match call {
        // ── signals ────────────────────────────────────────────────────────
        //
        // Dispatched here for the first time 2026-09-11. Every one of these
        // numbers is different on the two architectures (`akuma-syscalls-abi`'s
        // signal block says which), so the `to_glue` hop is doing real work: an
        // x86_64 `kill`(62) handed to glue unhopped is asm-generic
        // `sched_setaffinity`.
        //
        // `rt_sigaction` was `=> 0` and `rt_sigprocmask` a local arm that always
        // reported an empty mask — both honest while nothing was ever delivered,
        // and both wrong the moment something is: a program whose handler is not
        // recorded cannot have it run, and a program whose `SIG_BLOCK` is
        // discarded takes a signal it asked to defer.
        Syscall::RtSigaction
        | Syscall::RtSigprocmask
        | Syscall::Sigaltstack => to_glue(call, [a1, a2, a3, a4, a5, a6]),
        // `kill(2)` is this target's since 2026-10-09: glue's hands SIGKILL to
        // the AArch64 hard-kill path, which this kernel cannot survive under
        // load (`crate::signal::sys_kill` has the measurements). Every signal
        // is pended and the wait interrupted; `signal::deliver_pending` owns
        // the fatality, as it does for `tkill`.
        Syscall::Kill => crate::signal::sys_kill(a1, a2),
        // **Not** glue's, and `crate::signal::sys_tkill`'s doc comment is the
        // reason: glue decides a fatal default *inline* by calling
        // `sys_exit_group`, which on this target leaves ring 3 through the wrong
        // epilogue. Here every signal is pended and `signal::deliver_pending`
        // owns every fatality decision.
        Syscall::Tkill => crate::signal::sys_tkill(a1 as u32, a2 as u32),
        Syscall::Tgkill => crate::signal::sys_tgkill(a1 as u32, a2 as u32, a3 as u32),
        // Glue's: it takes from the same per-thread pending set
        // `crate::signal`'s delivery does (`threading::take_pending_signal`).
        Syscall::RtSigtimedwait => to_glue(call, [a1, a2, a3, a4, 0, 0]),
        // Local by necessity: the register file it restores is this target's
        // `UserCtx`, and glue's row is `=> 0` because on AArch64 `rt_sigreturn`
        // never reaches the dispatcher at all (the EL0 sync handler consumes it).
        Syscall::RtSigreturn => crate::signal::sys_rt_sigreturn(),
        // `rt_sigsuspend` is glue's, beside `pause` (34) above: it arms the
        // restore-sigmask slot, and `signal::deliver_pending` hands the saved
        // mask to the frame (or puts it back when no handler runs).
        Syscall::RtSigsuspend | Syscall::RtSigpending => to_glue(call, [a1, a2, 0, 0, 0, 0]),
        _ => return None,
    })
}

/// Clocks, sleeps, interval timers and `times`.
#[allow(clippy::inline_always)] // one switch, not eight calls: see the module doc
#[inline(always)]
fn time(call: Syscall, a: [u64; 6]) -> Option<u64> {
    let [a1, a2, a3, a4, _, _] = a;
    Some(match call {
        // `nanosleep(req, rem)` — **glue's** (C3, 2026-09-12).
        //
        // This was a bare `yield_now()` until 2026-09-06 — "no high-resolution
        // sleep: this target has a coarse, uncalibrated clock", which is true
        // and was the wrong conclusion. A `nanosleep` that returns immediately
        // is not a coarse sleep, it is **no sleep**, and every program that
        // uses one to sequence against another thread silently loses its
        // ordering. (Found by `scripts/futex_suite.py`'s `futexops`, which
        // reported a requeue bug that did not exist: the probe `nanosleep`s
        // three times for a second each and all three returned at once, so of
        // course the 400 ms park had not fired. The futex was correct and the
        // clock the probe steered by was not moving.)
        //
        // What replaced it was a yield-spin with `allow_tick`, and what
        // replaces *that* is `akuma_syscalls_time::sys_nanosleep`, which parks
        // the thread WAITING against a deadline (`schedule_blocking`) instead
        // of spinning — its x86 arm is the same `yield`-then-`allow_tick`
        // sequence, reached only when nothing else can take the core, and
        // `wake_pass` is what readies the sleeper at the deadline.
        //
        // Two checks came with the fold rather than being dropped at it: the
        // signal check this arm had (`^C` on a sleeping foreground job) is
        // `should_interrupt_blocking_syscall` there too, and the `EINVAL` for
        // a malformed interval moved *into* the shared crate, which did not
        // have it — see `Timespec::is_valid_interval`. The group-exit check
        // (`thread::should_leave_now`) does not survive as itself: glue's loop
        // asks `should_interrupt_blocking_syscall`, whose first half is the
        // same per-thread interrupt bit a group exit sets.
        //
        // `rem` is untouched on both paths out, there as here: Linux fills it
        // on an interrupted *relative* sleep and neither kernel does. The
        // divergence is pinned in the shared crate now instead of in each.
        Syscall::Nanosleep => to_glue(call, [a1, a2, 0, 0, 0, 0]),
        // **The clock family is glue's** (C3, 2026-09-12 —
        // `docs/archive/AKUMA_AMD64_C3_CLOCK.md`). Three arms that were a
        // second implementation of `akuma-syscalls-time`, kept here only
        // because that crate read its clock from `akuma_timer` — CNTVCT, which
        // is a stub returning `0` on this architecture. It reads the
        // boot-registered `akuma_primitives::clock` hook now, and the wall
        // clock both halves anchor is one static in the same place, so these
        // arms have nothing left to say that glue does not say better:
        //
        // - `clock_gettime` gains the large-`clock_id` guard (a pointer-sized
        //   id from a Go heap is `EINVAL` on Linux and used to copy a timespec
        //   to it here);
        // - `clock_settime` gains nothing and loses nothing — same two checks;
        // - `adjtimex` gains `ADJ_OFFSET` (this arm honoured only
        //   `ADJ_SETOFFSET`, so an `ntpd` asking for a slew got silence), the
        //   `ADJ_NANO` unit switch, and `STA_UNSYNC` in `status` — which is
        //   the honest answer from a machine with no frequency discipline and
        //   is what this arm reported `TIME_OK` with a zeroed status for.
        //
        // **One pinned divergence:** glue leaves `timex.tick` at `0` where
        // this arm reported `lapic::US_PER_TICK_TARGET`. Not restored on
        // purpose — the AArch64 tick is governor-tuned and can demote to 1 ms,
        // so a constant in the shared crate would be wrong on one of the two
        // kernels, and nothing in the tree reads the field.
        Syscall::ClockGettime => to_glue(call, [a1, a2, 0, 0, 0, 0]),
        Syscall::ClockSettime => to_glue(call, [a1, a2, 0, 0, 0, 0]),
        Syscall::Adjtimex => to_glue(call, [a1, 0, 0, 0, 0, 0]),
        // The rest of the family, which had no arm here at all: each was
        // `ENOSYS` on this target and served on the other one, because the
        // number to reach it by was missing from `akuma-syscalls-abi` rather
        // than the implementation being missing from glue (C3).
        //
        // `clock_getres` reports 1 us for every clock id — a claim this
        // target cannot make good on with a 10 ms tick, and one the AArch64
        // kernel makes too; pinned there, not re-litigated here.
        //
        // `clock_nanosleep` is what `std::thread::sleep` calls on any
        // `target_os = "linux"` build, plain `nanosleep` is not, and this
        // target runs Rust std binaries (`ruststd`). It carries
        // `TIMER_ABSTIME` and the CLOCK_REALTIME-to-uptime conversion with it.
        //
        // `setitimer` is the one with state behind it: see `check_itimers`,
        // driven from the tick in `idt::timer_dispatch`. Without that call the
        // arm would arm a timer nothing ever fires.
        //
        // `times`/`getrusage` zero their buffers and report uptime in clock
        // ticks. Nothing here accounts per-process CPU time yet, on either
        // kernel, and a zeroed `struct tms` is what shells read to print `0m0s`
        // rather than garbage.
        Syscall::ClockGetres => to_glue(call, [a1, a2, 0, 0, 0, 0]),
        Syscall::ClockNanosleep => to_glue(call, [a1, a2, a3, a4, 0, 0]),
        Syscall::ClockAdjtime => to_glue(call, [a1, a2, 0, 0, 0, 0]),
        Syscall::Setitimer => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        Syscall::Times => to_glue(call, [a1, 0, 0, 0, 0, 0]),
        _ => return None,
    })
}

/// The machine and the host: `uname`, hostnames, `sysinfo`, `syslog`, `reboot`,
/// `getrandom`, SysV message queues.
#[allow(clippy::inline_always)] // one switch, not eight calls: see the module doc
#[inline(always)]
fn system(call: Syscall, a: [u64; 6]) -> Option<u64> {
    let [a1, a2, a3, a4, a5, a6] = a;
    Some(match call {
        // `getrandom(buf, len, flags)` — **served by glue** (C1 step 3, batch 3).
        //
        // A leaf by `akuma_syscalls::fast_path`'s reckoning and the last one on
        // the C1 hand-off's step-3 list that could not be folded, because
        // glue's body named `akuma_virtio::rng::fill_bytes` outright and this
        // target has no virtio-rng on any rig — the answer would have been
        // `EIO` to every caller, `sshd`'s key exchange included. The seam is
        // `akuma_primitives::rng`, registered with `net::rng_fill_checked` in
        // `boot::install_shared_sinks`; glue still falls back to the virtio
        // device when nothing is registered, so the AArch64 kernel keeps the
        // behaviour it had.
        //
        // Two divergences close on the way, and both were silent here:
        // glue **loops** where this capped at one 256-byte chunk, and it
        // returns `EIO` on a short fill where this returned the byte count
        // regardless — so a `RDRAND` that ran out of entropy handed ring 3 a
        // buffer whose tail was kernel stack and called it random.
        Syscall::Getrandom => to_glue(call, [a1, a2, a3, a4, a5, a6]),
        // `sysinfo(struct sysinfo *)` — x86_64 99. `busybox free`/`top` read
        // total/free RAM from here, not from `/proc/meminfo`, and a missing one
        // means `used = total - free - …` underflows to 18 quintillion.
        Syscall::Sysinfo => sys_sysinfo(a1),
        // `syslog(type, buf, len)` — x86_64 103 (`klogctl`). Backed by the
        // console ring buffer in `serial.rs`, so `busybox dmesg` returns the
        // kernel's own boot/diagnostic output over ssh — the only way to read
        // it on the reference box, whose console is a write-only framebuffer.
        Syscall::Syslog => sys_syslog(a1, a2, a3),
        // `reboot(magic1, magic2, cmd, arg)` — x86_64 169. The ABI decode is
        // shared with the aarch64 kernel (`akuma-boot`); the x86 machine reset
        // under it is `reboot.rs`. `busybox reboot`/`halt`/`poweroff` all land
        // here.
        Syscall::Reboot => crate::reboot::sys_reboot(a1, a2, a3, a4),
        // uname(2) — **the first arm served by `akuma-syscalls-glue`** (C1 step 3).
        //
        // It was already "the same answer the aarch64 kernel gives, machine
        // string aside"; now it *is* that answer. The machine string was the one
        // real difference and it moved into glue as `UTS_MACHINE`, derived from
        // `target_arch` — a shared `uname` reporting `aarch64` on this box would
        // be a wrong answer nothing refuses.
        //
        // Two fields change what they print, deliberately: `release` and
        // `version` now come from glue's build identity (`<git-sha>-<profile>`)
        // rather than `banner::RELEASE`/`VERSION_DESC`. That is the fold working
        // — one answer, not two — and it is a gain: `uname -a` here now names the
        // commit it is running. `banner::print()` keeps the local strings for the
        // boot banner, which is where the target's name belongs.
        Syscall::Uname => to_glue(call, [a1, a2, a3, a4, a5, a6]),
        // sethostname/setdomainname — glue's, so `uname` and the setter share one
        // pair of fields. Absent from every table until 2026-10-15; OpenRC's
        // `hostname` service needs them.
        Syscall::Sethostname | Syscall::Setdomainname => {
            to_glue(call, [a1, a2, a3, a4, a5, a6])
        }
        // `msgget`/`msgctl`/`msgsnd`/`msgrcv` — SysV message queues, glue's
        // arms (`sc-sysv-ipc`). Queues are keyed `(box_id, msqid)`; this
        // target's processes all read `box_id == 0` (no "box"/namespace
        // concept here), so every process shares one queue namespace — the
        // correct single-box answer, not a divergence.
        Syscall::MsgGet => to_glue(call, [a1, a2, 0, 0, 0, 0]),
        Syscall::MsgCtl => to_glue(call, [a1, a2, a3, 0, 0, 0]),
        Syscall::MsgSnd => to_glue(call, [a1, a2, a3, a4, 0, 0]),
        Syscall::MsgRcv => to_glue(call, [a1, a2, a3, a4, a5, 0]),
        _ => return None,
    })
}

/// Hand a syscall to `akuma-syscalls-glue` — the implementation the AArch64
/// kernel serves it from.
///
/// **This is the C1 seam**, and the one place the number vocabulary changes.
/// Userspace passed an x86_64 number; glue's dispatch is a `match` over
/// asm-generic constants, so `to_aarch64()` is not a formality — handing glue
/// the number that arrived would find the *wrong* arm, not none
/// (`docs/archive/AKUMA_AMD64_C1_DISPATCH_VOCABULARY.md`). Taking a `Syscall`
/// rather than a `u64` is what makes that unskippable.
///
/// Three things had to exist before the first arm could come through here, none
/// of which the C1 hand-off prompt listed
/// (`docs/archive/AKUMA_AMD64_C1_STEP3_PREREQUISITES.md`):
/// `akuma_exec::runtime::register` (`exec_runtime.rs`), `stac`/`clac` in the
/// shared user-copy loop, and an x86 arm for `akuma_mmu::get_current_ttbr0`.
/// The prologue itself needed nothing: it reaches `akuma_exec::threading`,
/// which **is** `akuma-threading`, the scheduler this target has run since A1.
pub(super) fn to_glue(call: Syscall, args: [u64; 6]) -> u64 {
    akuma_syscalls_glue::handle_syscall(call.to_aarch64(), &args)
}

/// [`to_glue`]'s sibling for the shared-number syscalls — see that match
/// block's own header comment. There is no `Syscall` to hop through because
/// there is no hop: `nr` is already the number glue dispatches on.
pub(super) fn to_glue_raw(nr: u64, args: [u64; 6]) -> u64 {
    akuma_syscalls_glue::handle_syscall(nr, &args)
}

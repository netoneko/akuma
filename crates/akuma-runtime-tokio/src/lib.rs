//! The syscall surface a tokio program needs, as one dependency.
//!
//! A composition root that depends on this crate runs tokio: "this kernel runs
//! tokio" is a dependency that ensures Linux binary compatibility
//! (`docs/archive/TAOXICHUAN_CLEANUP_PLAN.md`, Phase 3; the analysis is
//! `docs/archive/AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md`).
//! It owns three things:
//!
//! - **The allow-list** ([`served_by_glue`]): which calls `akuma-syscalls-glue`
//!   answers, grouped by the tokio feature that needs them. A call outside it is
//!   the kernel's or nobody's.
//! - **The setup** ([`install`]): the hooks glue, `akuma-vfs-glue`, the pipe
//!   layer and the RNG read, with a tokio kernel's answers (no rump, no console
//!   poller, no mouse, tun, audio or file-page cache). The kernel supplies only
//!   what is genuinely its own: the clock, the core count, randomness, and what
//!   to do after a reap.
//! - **The dispatch** ([`serve`], [`serve_shared`]): an allow-listed call to glue
//!   by its asm-generic number. Arch-neutral: each kernel's front decodes its own
//!   numbers and routes what this answers `None` for to its ABI layer (the x86_64
//!   fd family is `akuma-syscalls-glue-amd64`'s; on AArch64 glue's own arms take
//!   the asm-generic calls as they are).
//!
//! What stays in the kernel: memory (`mmap` and friends), `clone`/`execve`/`exit`,
//! signal delivery and the frame, and any device of its own. Those are the
//! "requires of the kernel" in the analysis; they move here when glue owns one
//! implementation of each.
//!
//! Features mirror tokio's: `net` (default) and `process`. Turning one off takes
//! its calls off the allow-list, and `net` also takes glue's smoltcp socket
//! table out of the build. `shell` is the one that is not tokio's: the calls a
//! shell and its tools make and no tokio program does.

#![no_std]
#![forbid(unsafe_code)]

use akuma_syscalls_abi::Syscall;

pub mod console;

/// What the composition root supplies. Everything else a tokio kernel needs from
/// the hooks has one answer, which [`install`] gives.
#[derive(Clone, Copy)]
pub struct Kernel {
    /// Wall-clock microseconds since the epoch, `None` before the clock is set.
    pub utc_time_us: fn() -> Option<u64>,
    /// Cores the scheduler runs on (`sched_getaffinity`, `/proc/cpuinfo`).
    pub online_cpus: fn() -> usize,
    /// Fill the buffer with random bytes; `false` if it could not.
    pub rng_fill: fn(&mut [u8]) -> bool,
    /// Runs after every `wait4`/`waitid`: a kernel frees what a reap leaves
    /// (Taoxichuan's zombie roots).
    pub after_reap: fn(),
}

static KERNEL: akuma_primitives::OnceCopy<Kernel> = akuma_primitives::OnceCopy::new();

/// Register the kernel's half and install every hook a tokio kernel needs. Call
/// once, before the root is mounted (`akuma-vfs-glue` reads its hooks while
/// mounting) and before the first ring-3 crossing.
pub fn install(k: Kernel) {
    KERNEL.set(k);
    akuma_vfs_glue::set_hooks(akuma_vfs_glue::VfsGlueHooks {
        audio_is_available: || false,
        fs_exists: akuma_vfs_glue::exists,
        probed_core_count: k.online_cpus,
        utc_time_us: k.utc_time_us,
        // Glue's `fs::init` would call this; a tokio kernel mounts its root
        // itself and arms the file-page cache there (Taoxichuan: `mount_root`).
        fpcache_init: |_| {},
        // Nothing renders `/proc/<pid>/maps` rows from here.
        pid_map_rows: |_| None,
    });
    akuma_syscalls_glue::set_hooks(akuma_syscalls_glue::SyscallHooks {
        box_is_rump: |_| false,
        mark_box_rump: |_| {},
        attach_server: |_, _| {},
        intercept_box_syscall: |_, _| None,
        rump_socket_readable: |_| false,
        utc_time_us: k.utc_time_us,
        probed_core_count: k.online_cpus,
        poll_console_state: |_| None,
        dev_mouse_has_events: || false,
        dev_tun_has_frame: || false,
        release_shared_write: |_, _| {},
    });
    // Pipe wakes go to the thread by number, with a fresh handle: the handle a
    // parked writer recorded is not trusted to still name its slot.
    akuma_syscalls_glue::pipe::set_wake_sink(|tid, _handle| {
        akuma_exec::threading::wake_by_handle(akuma_exec::threading::wake_handle_for_thread(tid));
    });
    // Glue's `getrandom` reads the primitives hook.
    akuma_primitives::rng::set_rng_hook(k.rng_fill);
}

/// Serve `call` if it is on the allow-list: `Some` with the syscall-return value,
/// `None` for a call the kernel must answer itself (or route to its ABI layer).
///
/// Arch-neutral: the kernel's syscall front decodes its own numbers
/// (`Syscall::from_x86_64`, `Syscall::from_aarch64`) and hands the call here; glue
/// is reached by the asm-generic number on every architecture.
pub fn serve(call: Syscall, args: [u64; 6]) -> Option<u64> {
    if !served_by_glue(call) {
        return None;
    }
    let r = akuma_syscalls_glue::handle_syscall(call.to_aarch64(), &args);
    if matches!(call, Syscall::Wait4 | Syscall::Waitid)
        && let Some(k) = KERNEL.get()
    {
        (k.after_reap)();
    }
    Some(r)
}

/// Serve a shared-number call ([`shared_number`]), which no `Syscall` row names:
/// `nr` is the same on both ABIs, so a front asks this before it decodes.
#[must_use]
pub fn serve_shared(nr: u64, args: [u64; 6]) -> Option<u64> {
    shared_number(nr).then(|| akuma_syscalls_glue::handle_syscall(nr, &args))
}

/// Whether `nr` is one of this surface's shared-number calls.
///
/// `pidfd_open` (434) and `pidfd_send_signal` (424): Linux gave every call since
/// about 5.1 one number on every 64-bit architecture, so `akuma-syscalls-abi`
/// cannot name them (its rule 3) and they reach glue by the number itself, as
/// amd64's dispatcher sends them. A test pins that the numbers agree.
#[must_use]
pub const fn shared_number(nr: u64) -> bool {
    cfg!(feature = "process")
        && matches!(nr, akuma_syscalls_linux::nr::PIDFD_OPEN | akuma_syscalls_linux::nr::PIDFD_SEND_SIGNAL)
}

/// `ExecRuntime::pidfd_close` for a kernel on this surface: the last descriptor
/// naming a pidfd releases glue's record of it.
#[cfg(feature = "process")]
pub const PIDFD_CLOSE: fn(u32) = akuma_syscalls_glue::pidfd::pidfd_close;
/// Without `process` no pidfd is ever opened.
#[cfg(not(feature = "process"))]
pub const PIDFD_CLOSE: fn(u32) = |_| {};

/// Whether glue answers `call` on a kernel built with this crate's features.
#[must_use]
pub const fn served_by_glue(call: Syscall) -> bool {
    runtime(call)
        || (cfg!(feature = "net") && net(call))
        || (cfg!(feature = "process") && process(call))
        || (cfg!(feature = "shell") && shell(call))
}

/// What every Rust std + tokio binary calls, whatever its tokio features.
///
/// The 13 calls B10's census measured in every tokio program it ran: who it is,
/// the worker count and randomness, signal dispositions and masks (std ignores
/// SIGPIPE and installs its stack-overflow handler), thread naming and the exit
/// tid clear, the clock, and `futex` (std's locks and parking). Plus the two
/// sleeps, which no census program made (tokio sleeps in `epoll_pwait`'s
/// timeout) but `std::thread::sleep` does, from any blocking task.
#[must_use]
pub const fn runtime(call: Syscall) -> bool {
    use Syscall as S;
    matches!(
        call,
        S::Uname
            | S::Getpid
            | S::Gettid
            | S::Geteuid
            | S::SchedGetaffinity
            | S::Getrandom
            | S::RtSigaction
            | S::RtSigprocmask
            | S::Sigaltstack
            | S::Prctl
            | S::SetTidAddress
            | S::ClockGettime
            | S::Nanosleep
            | S::ClockNanosleep
            | S::Futex
    )
}

/// The `shell` feature: what busybox and bash call and no census program did.
///
/// Job control (`setsid`, `setpgid` and the getters), the ids `id` prints, limits
/// and accounting (`ulimit`, `time`, `uptime`), `sync`, `membarrier`, and the
/// setters behind `date -s` and `hostname`.
#[must_use]
pub const fn shell(call: Syscall) -> bool {
    use Syscall as S;
    matches!(
        call,
        S::Sethostname
            | S::Setdomainname
            | S::Getppid
            | S::Getuid
            | S::Getgid
            | S::Getegid
            | S::Getsid
            | S::Setsid
            | S::Getpgid
            | S::Setpgid
            | S::Membarrier
            | S::Sync
            | S::Syncfs
            | S::Sysinfo
            | S::Getrusage
            | S::Getgroups
            | S::Times
            | S::ClockGetres
            | S::ClockSettime
            | S::Adjtimex
            | S::ClockAdjtime
            | S::Prlimit64
            | S::Getrlimit
    )
}

/// tokio `net`: sockets. `sockaddr` and `msghdr` have one layout on both ABIs, so
/// these go to glue unchanged.
#[must_use]
pub const fn net(call: Syscall) -> bool {
    use Syscall as S;
    matches!(
        call,
        S::Socket
            | S::Bind
            | S::Listen
            | S::Accept
            | S::Accept4
            | S::Connect
            | S::Sendto
            | S::Recvfrom
            | S::Sendmsg
            | S::Recvmsg
            | S::Setsockopt
            | S::Getsockopt
            | S::Shutdown
            | S::Getsockname
            | S::Getpeername
    )
}

/// tokio `process`: reaping. `Child::wait` is `wait4` with `WNOHANG` after SIGCHLD.
#[must_use]
pub const fn process(call: Syscall) -> bool {
    matches!(call, Syscall::Wait4 | Syscall::Waitid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_a_group(c: Syscall) -> bool {
        runtime(c) || net(c) || process(c) || shell(c)
    }

    /// The groups together are exactly the list Taoxichuan's `served_by_glue`
    /// held when it moved here: a kernel with every feature routes as before.
    #[test]
    fn groups_are_taoxichuans_former_list() {
        let mut got: [u64; 64] = [0; 64];
        let mut n = 0;
        for &c in Syscall::ALL {
            if in_a_group(c) {
                got[n] = c.to_x86_64();
                n += 1;
            }
        }
        let got = &mut got[..n];
        got.sort_unstable();
        let mut want: [u64; 55] = [
            63, 170, 171, 39, 110, 186, 102, 107, 104, 108, 124, 112, 121, 109, 204, 318, 324, 162, 306, 99, 98, 115,
            100, 13, 14, 131, 157, 218, 228, 229, 227, 159, 305, 35, 230, 302, 97, 61, 247, 41, 49, 50, 43, 288, 42,
            44, 45, 46, 47, 54, 55, 48, 51, 52, 202,
        ];
        want.sort_unstable();
        assert_eq!(got, &want[..]);
    }

    /// No call is in two groups, so a feature's list is exactly what it adds.
    #[test]
    fn groups_are_disjoint() {
        for &c in Syscall::ALL {
            let k = u8::from(runtime(c)) + u8::from(net(c)) + u8::from(process(c)) + u8::from(shell(c));
            assert!(k <= 1, "{c:?} is in {k} groups");
        }
    }

    /// The allow-list follows the features it was built with.
    #[test]
    fn served_follows_features() {
        assert!(served_by_glue(Syscall::Futex));
        assert_eq!(served_by_glue(Syscall::Socket), cfg!(feature = "net"));
        assert_eq!(served_by_glue(Syscall::Wait4), cfg!(feature = "process"));
        assert_eq!(served_by_glue(Syscall::Setsid), cfg!(feature = "shell"));
    }

    /// `runtime` is B10's census (the x86_64 numbers every census program's
    /// calls on this crate's allow-list came to) plus the two sleeps.
    #[test]
    fn runtime_is_the_census() {
        let census = [13, 14, 39, 63, 107, 131, 157, 186, 202, 204, 218, 228, 318];
        let sleeps = [35, 230];
        for &c in Syscall::ALL {
            let nr = c.to_x86_64();
            assert_eq!(runtime(c), census.contains(&nr) || sleeps.contains(&nr), "{c:?}");
        }
    }

    /// The shared-number calls really are one number on both ABIs: x86_64's
    /// 434/424 (musl's `arch/x86_64/bits/syscall.h.in`) are the asm-generic
    /// constants glue dispatches on, and the abi table names neither.
    #[test]
    fn shared_numbers_agree() {
        assert_eq!(akuma_syscalls_linux::nr::PIDFD_OPEN, 434);
        assert_eq!(akuma_syscalls_linux::nr::PIDFD_SEND_SIGNAL, 424);
        for nr in [434, 424] {
            assert_eq!(Syscall::from_x86_64(nr), None);
            assert_eq!(shared_number(nr), cfg!(feature = "process"));
        }
        assert!(!shared_number(28));
    }

    /// Every served call round-trips through its x86_64 number, which is what
    /// the amd64 front decodes before [`serve`].
    #[test]
    fn served_calls_decode_from_x86_64() {
        for &c in Syscall::ALL {
            if in_a_group(c) {
                assert_eq!(Syscall::from_x86_64(c.to_x86_64()), Some(c));
            }
        }
    }

    /// And through its asm-generic number, which the AArch64 front decodes: the
    /// allow-list is one list of calls for both architectures.
    #[test]
    fn served_calls_decode_from_aarch64() {
        for &c in Syscall::ALL {
            if in_a_group(c) {
                assert_eq!(Syscall::from_aarch64(c.to_aarch64()), Some(c));
            }
        }
    }
}

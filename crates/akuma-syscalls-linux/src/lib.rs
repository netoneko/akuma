#![no_std]
// Padding and reserved fields keep the names the Linux headers give them —
// `__pad1`, `__unused`, `__spare0`, `_pad`. They have to be `pub` (callers
// build these structs with literal syntax) and they have to keep the leading
// underscore, because the whole value of this crate is that a field list here
// can be read side by side with the C one. `pub_underscore_fields` reads that
// combination as a mistake; here it is the point.
#![allow(clippy::pub_underscore_fields)]
//! The Linux/aarch64 syscall ABI: syscall numbers, the `repr(C)` structs that
//! cross the user/kernel boundary, and the flag tables — with the layout
//! assertions and host tests that pin them.
//!
//! # Why this is a crate
//!
//! `src/syscall/` is ~17k lines inside the **bin** crate, so none of it is
//! reachable from a library crate and none of it is host-testable. That is the
//! same failure `akuma_primitives::errno` was extracted to fix, and the
//! argument is quoted from `TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §5.7 without
//! change: *the table lived somewhere the other caller could not reach.*
//!
//! It had already produced the same result for struct layouts.
//! `struct timespec` existed five times in two representations when this crate
//! was written, and the two copies named `LocalTimespec` carried the reason in
//! the name — `akuma-time` **is** the timespec syscalls and could not reach the
//! bin crate's definition, so it made its own.
//!
//! The failure mode is worse here than for errno. A wrong errno is visible at
//! the call site. **A wrong field offset or flag bit does not crash, it
//! corrupts** — the class of bug a QEMU boot is worst at catching and a host
//! test is best at. Everything in this crate is checkable against a real Linux
//! header with no kernel state, no mocking and no VM, which is why the
//! assertions are `const _: () = assert!(...)` and the tests are plain
//! `cargo test`.
//!
//! # What is in it
//!
//! - [`nr`] — the syscall number table.
//! - [`flags`] — `O_*`, `AT_*`, `F_*`, `MAP_*`, `PROT_*`, `MADV_*`,
//!   `MREMAP_*`, `POLL*`, `EPOLL*`.
//! - the wire structs, re-exported flat at the crate root: [`Timespec`],
//!   [`Stat`], [`Statx`], [`MsgHdr`], [`KernelSigaction`], [`CloneArgs`] and
//!   the rest.
//!
//! # What is deliberately not in it
//!
//! - **errno.** Already extracted to `akuma_primitives::errno`, which is the
//!   right home. Depend on it; do not copy it here — a second copy is the exact
//!   mistake this crate exists to prevent.
//! - **Akuma-specific types** — `SpawnOptions`, `ThreadCpuStat`, the container
//!   syscalls. Those are Akuma ABI, not Linux ABI. The test for membership here
//!   is "can it be checked against a Linux header?", and they cannot.
//! - **`sockaddr_in` / `sockaddr_un`.** Already in `akuma-net`, already
//!   reachable. See [`net`]'s module docs.
//! - **Effects.** Nothing here reads or writes user memory, takes a lock, or
//!   touches a process. This crate is a leaf with no dependencies, and keeping
//!   it that way is what makes it free to depend on from anywhere.
//!
//! # Layout assertions are unconditional
//!
//! They are written for aarch64 LP64 and are not `cfg`-gated. A second
//! architecture is not planned, and gating them would mean the host test run —
//! the only place they are ever checked — skipped all of them.

pub mod flags;
pub mod io;
pub mod net;
pub mod nr;
pub mod proc;
pub mod signal;
pub mod stat;
pub mod time;

pub use io::{AioRingHeader, EpollEvent, IoVec, PollFd};
pub use net::{IfConfHdr, MsgHdr, SockAddrHw, Ucred};
pub use proc::{CloneArgs, Rlimit, Sysinfo};
pub use signal::{KernelSigaction, SigChld, Siginfo, StackT};
pub use stat::{Stat, Statfs, Statx, StatxTimestamp, makedev};
pub use time::{Itimerval, Timespec, Timeval, Timex};

#[cfg(test)]
mod tests {
    /// No two syscalls may share a number.
    ///
    /// The table was assembled by hand over ~200 entries and 23 of its members
    /// exist *because* the dispatch used to match raw numeric literals — the
    /// comment on those in `src/syscall/mod.rs` says it outright: "so a stray
    /// digit can't silently drift onto the wrong syscall the way
    /// `SCHED_SETSCHEDULER`'s body did". Nothing checked that claim until this
    /// test; a collision would route one syscall to another's handler, with no
    /// error anywhere.
    ///
    /// Kept as a literal list rather than generated, because a macro that
    /// builds the table from the same source it checks would prove nothing.
    #[test]
    fn no_two_syscall_numbers_collide() {
        use crate::nr::*;
        let table: &[(&str, u64)] = &[
            ("EXIT", EXIT), ("READ", READ), ("WRITE", WRITE), ("READV", READV),
            ("WRITEV", WRITEV), ("IOCTL", IOCTL), ("BRK", BRK), ("OPENAT", OPENAT),
            ("CLOSE", CLOSE), ("LSEEK", LSEEK), ("FSTAT", FSTAT),
            ("NANOSLEEP", NANOSLEEP), ("SOCKET", SOCKET), ("SOCKETPAIR", SOCKETPAIR),
            ("BIND", BIND), ("LISTEN", LISTEN), ("ACCEPT", ACCEPT),
            ("CONNECT", CONNECT), ("SENDTO", SENDTO), ("RECVFROM", RECVFROM),
            ("GETSOCKNAME", GETSOCKNAME), ("GETPEERNAME", GETPEERNAME),
            ("SETSOCKOPT", SETSOCKOPT), ("GETSOCKOPT", GETSOCKOPT),
            ("SHUTDOWN", SHUTDOWN), ("SENDMSG", SENDMSG), ("RECVMSG", RECVMSG),
            ("CLONE", CLONE), ("EXECVE", EXECVE), ("MUNMAP", MUNMAP),
            ("MREMAP", MREMAP), ("MMAP", MMAP), ("GETDENTS64", GETDENTS64),
            ("PSELECT6", PSELECT6), ("PPOLL", PPOLL), ("MKDIRAT", MKDIRAT),
            ("UNLINKAT", UNLINKAT), ("SYMLINKAT", SYMLINKAT), ("LINKAT", LINKAT),
            ("RENAMEAT", RENAMEAT), ("READLINKAT", READLINKAT),
            ("SET_TID_ADDRESS", SET_TID_ADDRESS), ("EXIT_GROUP", EXIT_GROUP),
            ("RT_SIGPROCMASK", RT_SIGPROCMASK), ("RT_SIGACTION", RT_SIGACTION),
            ("RT_SIGRETURN", RT_SIGRETURN), ("RT_SIGSUSPEND", RT_SIGSUSPEND),
            ("GETRANDOM", GETRANDOM), ("GETCWD", GETCWD), ("FCNTL", FCNTL),
            ("DUP", DUP), ("FSTATFS", FSTATFS), ("STATFS", STATFS), ("DUP3", DUP3),
            ("PIPE2", PIPE2), ("NEWFSTATAT", NEWFSTATAT), ("FACCESSAT", FACCESSAT),
            ("CLOCK_GETTIME", CLOCK_GETTIME), ("CLOCK_SETTIME", CLOCK_SETTIME),
            ("ADJTIMEX", ADJTIMEX), ("CLOCK_ADJTIME", CLOCK_ADJTIME),
            ("REBOOT", REBOOT), ("CLONE3", CLONE3), ("FACCESSAT2", FACCESSAT2),
            ("WAIT4", WAIT4), ("WAITID", WAITID), ("RESOLVE_HOST", RESOLVE_HOST),
            ("SPAWN", SPAWN), ("KILL", KILL), ("WAITPID", WAITPID), ("TIME", TIME),
            ("CHDIR", CHDIR),
            ("SET_TERMINAL_ATTRIBUTES", SET_TERMINAL_ATTRIBUTES),
            ("GET_TERMINAL_ATTRIBUTES", GET_TERMINAL_ATTRIBUTES),
            ("SET_CURSOR_POSITION", SET_CURSOR_POSITION), ("HIDE_CURSOR", HIDE_CURSOR),
            ("SHOW_CURSOR", SHOW_CURSOR), ("CLEAR_SCREEN", CLEAR_SCREEN),
            ("POLL_INPUT_EVENT", POLL_INPUT_EVENT), ("GET_CPU_STATS", GET_CPU_STATS),
            ("SPAWN_EXT", SPAWN_EXT), ("REGISTER_BOX", REGISTER_BOX),
            ("KILL_BOX", KILL_BOX), ("REATTACH", REATTACH), ("UPTIME", UPTIME),
            ("SET_TPIDR_EL0", SET_TPIDR_EL0), ("FB_INIT", FB_INIT),
            ("FB_DRAW", FB_DRAW), ("FB_INFO", FB_INFO),
            ("SET_BOX_STACK", SET_BOX_STACK), ("CLOSE_CHILD_STDIN", CLOSE_CHILD_STDIN),
            ("CORE_INIT", CORE_INIT), ("GETPID", GETPID), ("GETPPID", GETPPID),
            ("GETUID", GETUID), ("GETEUID", GETEUID), ("GETGID", GETGID),
            ("GETEGID", GETEGID), ("GETTID", GETTID), ("GETRESUID", GETRESUID),
            ("GETRESGID", GETRESGID), ("GETGROUPS", GETGROUPS),
            ("KILL_LINUX", KILL_LINUX), ("SETPGID", SETPGID), ("GETPGID", GETPGID),
            ("SETSID", SETSID), ("UNAME", UNAME), ("FLOCK", FLOCK), ("UMASK", UMASK),
            ("UTIMENSAT", UTIMENSAT), ("FDATASYNC", FDATASYNC), ("FSYNC", FSYNC),
            ("FCHDIR", FCHDIR), ("FCHMOD", FCHMOD), ("FCHMODAT", FCHMODAT),
            ("FCHOWNAT", FCHOWNAT), ("FCHOWN", FCHOWN), ("FTRUNCATE", FTRUNCATE),
            ("FALLOCATE", FALLOCATE), ("MADVISE", MADVISE), ("MPROTECT", MPROTECT),
            ("FUTEX", FUTEX), ("SET_ROBUST_LIST", SET_ROBUST_LIST),
            ("SIGALTSTACK", SIGALTSTACK), ("GETRLIMIT", GETRLIMIT),
            ("PRLIMIT64", PRLIMIT64), ("EVENTFD2", EVENTFD2), ("PREAD64", PREAD64),
            ("PWRITE64", PWRITE64), ("PREADV", PREADV), ("PWRITEV", PWRITEV),
            ("PREADV2", PREADV2), ("PWRITEV2", PWRITEV2), ("SETITIMER", SETITIMER),
            ("MEMBARRIER", MEMBARRIER), ("RT_SIGTIMEDWAIT", RT_SIGTIMEDWAIT),
            ("PRCTL", PRCTL), ("GETRUSAGE", GETRUSAGE), ("MSYNC", MSYNC),
            ("PROCESS_VM_READV", PROCESS_VM_READV),
            ("SCHED_SETAFFINITY", SCHED_SETAFFINITY),
            ("SCHED_GETAFFINITY", SCHED_GETAFFINITY), ("TKILL", TKILL),
            ("TGKILL", TGKILL), ("PIDFD_OPEN", PIDFD_OPEN),
            ("CLOSE_RANGE", CLOSE_RANGE), ("SYSINFO", SYSINFO),
            ("CLOCK_GETRES", CLOCK_GETRES), ("CLOCK_NANOSLEEP", CLOCK_NANOSLEEP),
            ("EPOLL_CREATE1", EPOLL_CREATE1), ("EPOLL_CTL", EPOLL_CTL),
            ("EPOLL_PWAIT", EPOLL_PWAIT), ("TIMERFD_CREATE", TIMERFD_CREATE),
            ("TIMERFD_SETTIME", TIMERFD_SETTIME), ("TIMERFD_GETTIME", TIMERFD_GETTIME),
            ("CAPGET", CAPGET), ("CAPSET", CAPSET), ("SETGID", SETGID),
            ("SETUID", SETUID), ("SETRESUID", SETRESUID), ("SETRESGID", SETRESGID),
            ("SETGROUPS", SETGROUPS), ("IO_URING_SETUP", IO_URING_SETUP),
            ("IO_URING_ENTER", IO_URING_ENTER),
            ("IO_URING_REGISTER", IO_URING_REGISTER),
            ("INOTIFY_INIT1", INOTIFY_INIT1),
            ("INOTIFY_ADD_WATCH", INOTIFY_ADD_WATCH),
            ("INOTIFY_RM_WATCH", INOTIFY_RM_WATCH), ("ACCEPT4", ACCEPT4),
            ("TIMES", TIMES), ("MOUNT", MOUNT), ("UMOUNT2", UMOUNT2),
            ("MOUNT_IN_NS", MOUNT_IN_NS), ("RENAMEAT2", RENAMEAT2), ("STATX", STATX),
            ("TRUNCATE", TRUNCATE), ("MSGGET", MSGGET), ("MSGCTL", MSGCTL),
            ("MSGRCV", MSGRCV), ("MSGSND", MSGSND), ("IO_SETUP", IO_SETUP),
            ("IO_DESTROY", IO_DESTROY), ("IO_SUBMIT", IO_SUBMIT),
            ("IO_CANCEL", IO_CANCEL), ("IO_GETEVENTS", IO_GETEVENTS),
            ("SETXATTR", SETXATTR), ("FREMOVEXATTR", FREMOVEXATTR),
            ("SCHED_SETPARAM", SCHED_SETPARAM),
            ("SCHED_SETSCHEDULER", SCHED_SETSCHEDULER),
            ("SCHED_YIELD", SCHED_YIELD), ("RESTART_SYSCALL", RESTART_SYSCALL),
            ("SETPRIORITY", SETPRIORITY), ("GETPRIORITY", GETPRIORITY),
        ];

        for (i, (name_a, nr_a)) in table.iter().enumerate() {
            for (name_b, nr_b) in &table[i + 1..] {
                assert_ne!(nr_a, nr_b, "{name_a} and {name_b} share syscall number {nr_a}");
            }
        }
    }

    /// The Akuma-private syscalls occupy 300–327 and must not collide with the
    /// Linux numbers they sit among — 300 is unused on aarch64 Linux, but the
    /// range is not reserved and nothing but this test says so.
    #[test]
    fn akuma_private_syscalls_stay_in_their_block() {
        use crate::nr::*;
        for nr in [
            RESOLVE_HOST, SPAWN, KILL, WAITPID, TIME, SET_TERMINAL_ATTRIBUTES,
            GET_TERMINAL_ATTRIBUTES, SET_CURSOR_POSITION, HIDE_CURSOR, SHOW_CURSOR,
            CLEAR_SCREEN, POLL_INPUT_EVENT, GET_CPU_STATS, SPAWN_EXT, REGISTER_BOX,
            KILL_BOX, REATTACH, UPTIME, SET_TPIDR_EL0, FB_INIT, FB_DRAW, FB_INFO,
            SET_BOX_STACK, MOUNT_IN_NS, CLOSE_CHILD_STDIN, CORE_INIT,
        ] {
            assert!((300..=327).contains(&nr), "{nr} is outside the Akuma block");
        }
    }

    /// The extended-attribute family is dispatched as one inclusive range
    /// (`SETXATTR..=FREMOVEXATTR`), so the endpoints have to bracket exactly the
    /// twelve calls that share the `EOPNOTSUPP` body — and nothing else.
    #[test]
    fn xattr_range_is_exactly_twelve_calls() {
        assert_eq!(crate::nr::FREMOVEXATTR - crate::nr::SETXATTR + 1, 12);
    }
}

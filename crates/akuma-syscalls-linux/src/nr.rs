//! Syscall numbers (Linux `asm-generic` / aarch64), plus the Akuma-private
//! extensions in the 300 block.
//!
//! Moved out of `src/syscall/mod.rs` 2026-08-27. It sat in the bin crate for
//! the same reason the errno table did — and with the same consequence: no
//! library crate could name a syscall it implements. `akuma-time` owns six of
//! the numbers below and could not reach any of them.
//!
//! **The `#[cfg(feature = ...)]` gates the bin crate's copy carried are gone.**
//! A syscall number is a fact about Linux, not about which features this build
//! compiles in; gating the *constant* only meant that turning `sc-timerfd` off
//! also erased the knowledge that 85 means `timerfd_create`. The dispatch arms
//! in `src/syscall/mod.rs` stay gated — that is where the feature actually
//! decides something.
pub const EXIT: u64 = 93;
pub const READ: u64 = 63;
pub const WRITE: u64 = 64;
pub const READV: u64 = 65;
pub const WRITEV: u64 = 66;
pub const IOCTL: u64 = 29;
pub const BRK: u64 = 214;
pub const OPENAT: u64 = 56;
pub const CLOSE: u64 = 57;
pub const LSEEK: u64 = 62;
pub const FSTAT: u64 = 80;
pub const NANOSLEEP: u64 = 101;
pub const SOCKET: u64 = 198;
pub const SOCKETPAIR: u64 = 199;
pub const BIND: u64 = 200;
pub const LISTEN: u64 = 201;
pub const ACCEPT: u64 = 202;
pub const CONNECT: u64 = 203;
pub const SENDTO: u64 = 206;
pub const RECVFROM: u64 = 207;
pub const GETSOCKNAME: u64 = 204;
pub const GETPEERNAME: u64 = 205;
pub const SETSOCKOPT: u64 = 208;
pub const GETSOCKOPT: u64 = 209;
pub const SHUTDOWN: u64 = 210;
pub const SENDMSG: u64 = 211;
pub const RECVMSG: u64 = 212;
pub const CLONE: u64 = 220;
pub const EXECVE: u64 = 221;
pub const MUNMAP: u64 = 215;
pub const MREMAP: u64 = 216;
pub const MMAP: u64 = 222;
pub const GETDENTS64: u64 = 61;
pub const PSELECT6: u64 = 72;
pub const PPOLL: u64 = 73;
pub const MKDIRAT: u64 = 34;
pub const UNLINKAT: u64 = 35;
pub const SYMLINKAT: u64 = 36;
pub const LINKAT: u64 = 37;
pub const RENAMEAT: u64 = 38;
pub const READLINKAT: u64 = 78;
pub const SET_TID_ADDRESS: u64 = 96;
pub const EXIT_GROUP: u64 = 94;
pub const RT_SIGPROCMASK: u64 = 135;
pub const RT_SIGACTION: u64 = 134;
pub const RT_SIGRETURN: u64 = 139;
pub const RT_SIGSUSPEND: u64 = 133;
pub const GETRANDOM: u64 = 278;
pub const GETCWD: u64 = 17;
pub const FCNTL: u64 = 25;
pub const DUP: u64 = 23;
pub const FSTATFS: u64 = 44;
pub const STATFS: u64 = 43;
pub const DUP3: u64 = 24;
pub const PIPE2: u64 = 59;
pub const NEWFSTATAT: u64 = 79;
pub const FACCESSAT: u64 = 48;
pub const CLOCK_GETTIME: u64 = 113;
/// See `docs/archive/MISSING_NTP_SYSCALLS.md`: `clock_settime`/`adjtimex`/
/// `clock_adjtime` were the missing half of clock support — `clock_gettime`
/// alone gives no way to ever correct a wrong clock.
pub const CLOCK_SETTIME: u64 = 112;
pub const ADJTIMEX: u64 = 171;
pub const CLOCK_ADJTIME: u64 = 266;
pub const REBOOT: u64 = 142;
pub const CLONE3: u64 = 435;
pub const FACCESSAT2: u64 = 439;
pub const WAIT4: u64 = 260;
pub const WAITID: u64 = 95;
pub const RESOLVE_HOST: u64 = 300;
pub const SPAWN: u64 = 301;
pub const KILL: u64 = 302;
pub const WAITPID: u64 = 303;
pub const TIME: u64 = 305;
pub const CHDIR: u64 = 49;
pub const SET_TERMINAL_ATTRIBUTES: u64 = 307;
pub const GET_TERMINAL_ATTRIBUTES: u64 = 308;
pub const SET_CURSOR_POSITION: u64 = 309;
pub const HIDE_CURSOR: u64 = 310;
pub const SHOW_CURSOR: u64 = 311;
pub const CLEAR_SCREEN: u64 = 312;
pub const POLL_INPUT_EVENT: u64 = 313;
pub const GET_CPU_STATS: u64 = 314;
pub const SPAWN_EXT: u64 = 315;
pub const REGISTER_BOX: u64 = 316;
pub const KILL_BOX: u64 = 317;
pub const REATTACH: u64 = 318;
pub const UPTIME: u64 = 319;
pub const SET_TPIDR_EL0: u64 = 320;
pub const FB_INIT: u64 = 321;
pub const FB_DRAW: u64 = 322;
pub const FB_INFO: u64 = 323;
/// Select a box's network stack.
///
/// arg0 = box_id, arg1 = stack (0 = smoltcp, 1 = rump). herd calls this for a
/// `stack = rump` service so the kernel routes that box's AF_INET syscalls to
/// its rump_server (RUMP_SYSPROXY.md).
pub const SET_BOX_STACK: u64 = 324;
/// Deliver EOF to a spawned child's stdin: arg0 = child pid.
///
/// Lets a parent (e.g. the userspace SSH-into-box bridge) signal "no more
/// input" so a shell reading a piped script (busybox `sh`) finishes reading
/// and runs to completion. Mirrors the in-kernel sshd's `close_process_stdin`
/// on CHANNEL_EOF; only the spawner may close its child's stdin.
pub const CLOSE_CHILD_STDIN: u64 = 326;
/// Permanent ENOSYS stub.
///
/// Activated a parked secondary core in the removed one-kernel-per-core
/// multikernel (docs/archive/TRIM_FAT_MULTIKERNEL.md). herd still calls this
/// to try pinning a service to a secondary core.
pub const CORE_INIT: u64 = 327;
pub const GETPID: u64 = 172;
pub const GETPPID: u64 = 173;
pub const GETUID: u64 = 174;
pub const GETEUID: u64 = 175;
pub const GETGID: u64 = 176;
pub const GETEGID: u64 = 177;
pub const GETTID: u64 = 178;
/// The three credential *queries* `setpriv` makes before dropping privileges.
///
/// Everything runs as root here, so all three answer 0 — but they have to
/// answer: ENOSYS made util-linux's `setpriv` bail ("getresuid failed:
/// Function not implemented"), which killed `redis:alpine`'s
/// `docker-entrypoint.sh` under its `set -e` (docs/archive/DEVBOX_ISSUES.md
/// Issue 15).
pub const GETRESUID: u64 = 148;
pub const GETRESGID: u64 = 150;
pub const GETGROUPS: u64 = 158;
pub const KILL_LINUX: u64 = 129;
pub const SETPGID: u64 = 154;
pub const GETPGID: u64 = 155;
pub const SETSID: u64 = 157;
pub const UNAME: u64 = 160;
pub const FLOCK: u64 = 32;
pub const UMASK: u64 = 166;
pub const UTIMENSAT: u64 = 88;
pub const FDATASYNC: u64 = 83;
pub const FSYNC: u64 = 82;
pub const FCHDIR: u64 = 50;
pub const FCHMOD: u64 = 52;
pub const FCHMODAT: u64 = 53;
pub const FCHOWNAT: u64 = 54;
pub const FCHOWN: u64 = 55;
pub const FTRUNCATE: u64 = 46;
pub const FALLOCATE: u64 = 47;
pub const MADVISE: u64 = 233;
pub const MPROTECT: u64 = 226;
pub const FUTEX: u64 = 98;
pub const SET_ROBUST_LIST: u64 = 99;
pub const SIGALTSTACK: u64 = 132;
pub const GETRLIMIT: u64 = 163;
pub const PRLIMIT64: u64 = 261;
pub const EVENTFD2: u64 = 19;
pub const PREAD64: u64 = 67;
pub const PWRITE64: u64 = 68;
pub const PREADV: u64 = 69;
pub const PWRITEV: u64 = 70;
pub const PREADV2: u64 = 286;
pub const PWRITEV2: u64 = 287;
pub const SETITIMER: u64 = 103;
pub const MEMBARRIER: u64 = 283;
pub const RT_SIGTIMEDWAIT: u64 = 137;
pub const PRCTL: u64 = 167;
pub const GETRUSAGE: u64 = 165;
pub const MSYNC: u64 = 227;
pub const PROCESS_VM_READV: u64 = 270;
pub const SCHED_SETAFFINITY: u64 = 122;
pub const SCHED_GETAFFINITY: u64 = 123;
pub const TKILL: u64 = 130;
pub const TGKILL: u64 = 131;
pub const PIDFD_OPEN: u64 = 434;
pub const CLOSE_RANGE: u64 = 436;
pub const SYSINFO: u64 = 179;
pub const CLOCK_GETRES: u64 = 114;
pub const CLOCK_NANOSLEEP: u64 = 115;
pub const EPOLL_CREATE1: u64 = 20;
pub const EPOLL_CTL: u64 = 21;
pub const EPOLL_PWAIT: u64 = 22;
pub const TIMERFD_CREATE: u64 = 85;
pub const TIMERFD_SETTIME: u64 = 86;
pub const TIMERFD_GETTIME: u64 = 87;
pub const CAPGET: u64 = 90;
/// The credential *setters*, as accepting no-ops.
///
/// This kernel has one identity — root — and no capability model, so there is
/// nothing to change and nothing to drop. They exist because privilege-dropping
/// wrappers treat a failure here as fatal: `redis:alpine`'s entrypoint runs
/// `setpriv --reuid=redis --regid=redis --clear-groups`, whose libcap-ng
/// `capng_apply` calls `capset` and whose `--clear-groups` calls `setgroups`.
/// ENOSYS from either killed the container before `redis-server` started
/// (docs/archive/DEVBOX_ISSUES.md Issue 15).
///
/// **Read this as "privilege dropping is not implemented", not "it worked".**
/// A caller that asks to become an unprivileged user stays root, silently. That
/// is the same fiction `getuid`/`geteuid` already tell (both hardcode 0); making
/// these fail instead would not add safety, only break the callers. A real
/// implementation needs per-process credentials first — there is no uid field on
/// `Process` to set.
pub const CAPSET: u64 = 91;
pub const SETGID: u64 = 144;
pub const SETUID: u64 = 146;
pub const SETRESUID: u64 = 147;
pub const SETRESGID: u64 = 149;
pub const SETGROUPS: u64 = 159;
pub const IO_URING_SETUP: u64 = 425;
pub const IO_URING_ENTER: u64 = 426;
pub const IO_URING_REGISTER: u64 = 427;
pub const INOTIFY_INIT1: u64 = 26;
pub const INOTIFY_ADD_WATCH: u64 = 27;
pub const INOTIFY_RM_WATCH: u64 = 28;
pub const ACCEPT4: u64 = 242;
pub const TIMES: u64 = 153;
pub const MOUNT: u64 = 40;
pub const UMOUNT2: u64 = 39;
pub const MOUNT_IN_NS: u64 = 325;
pub const RENAMEAT2: u64 = 276;
pub const STATX: u64 = 291;
pub const TRUNCATE: u64 = 45;
pub const MSGGET: u64 = 186;
pub const MSGCTL: u64 = 187;
pub const MSGRCV: u64 = 188;
pub const MSGSND: u64 = 189;

// The 23 syscalls `handle_syscall` used to dispatch by raw numeric literal
// instead of a name — named here so a stray digit can't silently drift onto
// the wrong syscall the way `SCHED_SETSCHEDULER`'s body did (see its call site).
pub const IO_SETUP: u64 = 0;
pub const IO_DESTROY: u64 = 1;
pub const IO_SUBMIT: u64 = 2;
pub const IO_CANCEL: u64 = 3;
pub const IO_GETEVENTS: u64 = 4;
/// First of the extended-attributes family (`setxattr`..`fremovexattr`,
/// 5-16) — all twelve are matched as one inclusive range,
/// `SETXATTR..=FREMOVEXATTR`, since they share one `EOPNOTSUPP` body.
pub const SETXATTR: u64 = 5;
pub const FREMOVEXATTR: u64 = 16;
pub const SCHED_SETPARAM: u64 = 118;
pub const SCHED_SETSCHEDULER: u64 = 119;
pub const SCHED_YIELD: u64 = 124;
pub const RESTART_SYSCALL: u64 = 128;
pub const SETPRIORITY: u64 = 140;
pub const GETPRIORITY: u64 = 141;

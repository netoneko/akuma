#![no_std]
// Nothing here touches memory: two number tables and the mapping between them.
#![forbid(unsafe_code)]
//! Which syscall a number means, **on which architecture**.
//!
//! Proposal item 5 (`docs/archive/REDUCING_PLATFORM_DEPENDENCY.md` §5), started
//! 2026-09-04 because the amd64 port made it stop being cosmetic.
//!
//! # Why this is a separate crate
//!
//! `akuma-syscalls-linux` describes itself as *"The Linux/aarch64 syscall
//! ABI"*, and that precision is the point of it — its numbers, wire structs and
//! flag tables are all one architecture's. Adding a second architecture's
//! numbering **inside** it would quietly make the crate's own name wrong, and
//! the next reader would have no way to tell which table a bare `nr::WRITE`
//! meant.
//!
//! So the arch-plural concept lives here instead, one level up: this crate
//! *reads* `akuma-syscalls-linux::nr` for the asm-generic numbers rather than
//! copying them, so the two can never drift, and owns the x86_64 table that has
//! no home down there.
//!
//! # The problem, concretely
//!
//! `nr`'s header says *"a syscall number is a fact about Linux, not about which
//! features this build compiles in"*, and that was right. But it is a fact about
//! Linux **on a particular architecture**, and the module name does not say
//! which. It is `asm-generic`, which is what aarch64 uses. x86_64 predates
//! `asm-generic` and has its own table:
//!
//! | | aarch64 (`asm-generic`) | x86_64 |
//! |---|---:|---:|
//! | `read` | 63 | **0** |
//! | `write` | 64 | **1** |
//! | `exit` | 93 | **60** |
//! | `exit_group` | 94 | **231** |
//! | `mmap` | 222 | **9** |
//! | `openat` | 56 | **257** |
//!
//! Note `read`: `0` on x86_64 is `io_setup` under `asm-generic`. A dispatcher
//! using the wrong table would not fail to find a handler — it would find the
//! **wrong** handler, which `akuma-syscalls-linux`'s own header calls out as the
//! failure mode worse than a crash.
//!
//! # Shape
//!
//! [`Syscall`] is the architecture-neutral name; [`Syscall::from_x86_64`] and
//! [`Syscall::from_aarch64`] are the two decodes. The raw constants stay exactly
//! where they are — they are still the wire facts, and `akuma-syscalls-glue`'s
//! 192 references to `nr::` are untouched. What changes is that a *new* caller
//! can dispatch on a name instead of a number.
//!
//! # One table, four views
//!
//! Widened 2026-09-07 from 36 variants to 80 for **C1**, the fold of
//! `amd64/src/usermode.rs` into `akuma-syscalls-glue`
//! (`proposals/NEXT_AGENT_AMD64_C1_USERMODE_FOLD.md`). Glue dispatches on
//! asm-generic numbers and the amd64 kernel is handed x86_64 ones, so something
//! has to translate; this enum is that translation, applied once at the amd64
//! syscall boundary as `Syscall::from_x86_64(nr)` … `to_aarch64()`.
//!
//! **Glue's own `match syscall_num` is deliberately NOT changed.** Making it
//! dispatch on this enum would be more honest and would also rewrite the
//! AArch64 kernel's syscall hot path, which nothing in C1 needs — and it would
//! throw away the cheapest proof that the AArch64 kernel is untouched by the
//! port (a byte-identical `.text`).
//!
//! The consequence, pinned rather than hidden: **inside glue the number is the
//! asm-generic one even on x86_64**, so `CURRENT_SYSCALL_NR` and glue's traces
//! read `64` where an x86_64 `strace` would say `1`. The amd64 kernel's own
//! trace keeps the number userspace actually passed, so the hop is visible in a
//! log rather than silent.
//!
//! The alternative shape — `#[cfg(target_arch)]` over `nr`'s 191 constants — was
//! rejected for a reason worth recording: `cfg!(target_arch)` resolves to the
//! **host** under `cargo test`, so on an x86_64 developer machine `nr::WRITE`
//! would silently become `1` and every host test of the AArch64 tables would be
//! testing the other architecture.
//!
//! Every entry is one row of [`syscall_table!`], which generates the enum, both
//! decodes, both encodes and [`Syscall::ALL`] together. That is the whole point
//! of the macro: a transposed digit can only be transposed once, and there is no
//! second list to forget to update.
//!
//! # Scope, and how to grow it
//!
//! Deliberately a **subset**: an entry is a claim that *both* tables were
//! checked, and `tables_disagree_where_linux_does` is what makes that claim
//! testable. Two rules keep it honest:
//!
//! 1. **Add a row when a caller needs it**, not speculatively. It is not a
//!    mirror of `nr` and should not become one.
//! 2. **A row means the call exists on both architectures with the same
//!    meaning.** The x86-only legacy spellings — `open`(2), `stat`(4),
//!    `lstat`(6), `poll`(7), `access`(21), `pipe`(22), `select`(23), `dup2`(33),
//!    `fork`(57), `vfork`(58), `rename`(82), `mkdir`(83), `rmdir`(84),
//!    `unlink`(87), `symlink`(88), `readlink`(89), `gettimeofday`(96),
//!    `getpgrp`(111), `arch_prctl`(158), `settimeofday`(164), `time`(201) —
//!    have **no** asm-generic number and must not get invented ones. They stay
//!    where they belong: `AT_FDCWD` shims in `amd64/src/usermode.rs`, each
//!    forwarding to the `*at` call this table does name.

use akuma_syscalls_linux::nr;

/// The single source of truth: one row per syscall, four views generated.
///
/// A row is `Variant => X86_NAME = <x86_64 number>, nr::AARCH64_NAME;` — the
/// x86_64 side is a literal because this crate owns that table, and the aarch64
/// side is a *path into* `akuma-syscalls-linux` because that crate owns its own
/// and the two must not be able to drift.
///
/// Both numbers appear in pattern position as well as expression position, so a
/// number used twice is an `unreachable_patterns` warning rather than a silent
/// wrong answer. That is deliberate — it is the whole failure mode this crate
/// exists to prevent.
macro_rules! syscall_table {
    ($(
        $(#[$attr:meta])*
        $variant:ident => $konst:ident = $x86:literal, $aarch64:path;
    )*) => {
        /// x86_64 Linux numbers.
        ///
        /// From `arch/x86/entry/syscalls/syscall_64.tbl`. Kept in its own module
        /// rather than merged into `akuma_syscalls_linux::nr` so that neither
        /// table can be reached by accident: a caller has to name the
        /// architecture it means.
        pub mod x86_64 {
            $( pub const $konst: u64 = $x86; )*
        }

        /// A syscall, named rather than numbered.
        #[derive(Copy, Clone, PartialEq, Eq, Debug)]
        #[non_exhaustive]
        pub enum Syscall {
            $( $(#[$attr])* $variant, )*
        }

        impl Syscall {
            /// Every syscall this table names, in table order.
            ///
            /// Generated from the same rows as the encodes and decodes, so an
            /// exhaustiveness test over it cannot go stale the way a
            /// hand-maintained list does.
            pub const ALL: &'static [Self] = &[ $( Self::$variant, )* ];

            /// Decode an x86_64 Linux syscall number.
            #[must_use]
            pub const fn from_x86_64(nr: u64) -> Option<Self> {
                Some(match nr {
                    $( $x86 => Self::$variant, )*
                    _ => return None,
                })
            }

            /// Decode an aarch64 (`asm-generic`) Linux syscall number.
            #[must_use]
            pub const fn from_aarch64(nr: u64) -> Option<Self> {
                Some(match nr {
                    $( $aarch64 => Self::$variant, )*
                    _ => return None,
                })
            }

            /// This syscall's x86_64 number.
            ///
            /// Infallible, and that is the invariant: a variant only exists here
            /// if it has a number on **both** architectures. The table enforces
            /// it — a row without both numbers does not parse.
            #[must_use]
            pub const fn to_x86_64(self) -> u64 {
                match self { $( Self::$variant => $x86, )* }
            }

            /// This syscall's aarch64 (`asm-generic`) number. Infallible; see
            /// [`Syscall::to_x86_64`].
            #[must_use]
            pub const fn to_aarch64(self) -> u64 {
                match self { $( Self::$variant => $aarch64, )* }
            }
        }
    };
}

syscall_table! {
    // ── I/O ────────────────────────────────────────────────────────────────
    Read       => READ       = 0,   nr::READ;
    Write      => WRITE      = 1,   nr::WRITE;
    Close      => CLOSE      = 3,   nr::CLOSE;
    Fstat      => FSTAT      = 5,   nr::FSTAT;
    Lseek      => LSEEK      = 8,   nr::LSEEK;
    Ioctl      => IOCTL      = 16,  nr::IOCTL;
    /// Positional read. Added 2026-09-07 for the amd64 port, which had no arm
    /// for it at all — `mmapsum`'s `read()` reference arm aborted at offset 0
    /// and every archive reader and `rustc` metadata load goes through it.
    Pread64    => PREAD64    = 17,  nr::PREAD64;
    Readv      => READV      = 19,  nr::READV;
    Writev     => WRITEV     = 20,  nr::WRITEV;
    /// x86_64 32 is `dup` and asm-generic 32 is `flock`; the pair is one of the
    /// crossings `collisions_that_would_corrupt` pins.
    Dup        => DUP        = 32,  nr::DUP;
    Dup3       => DUP3       = 292, nr::DUP3;
    Pipe2      => PIPE2      = 293, nr::PIPE2;
    Fcntl      => FCNTL      = 72,  nr::FCNTL;
    /// x86_64 73 is `flock` and asm-generic 73 is `ppoll` — see [`Syscall::Ppoll`].
    Flock      => FLOCK      = 73,  nr::FLOCK;
    Getdents64 => GETDENTS64 = 217, nr::GETDENTS64;
    Getcwd     => GETCWD     = 79,  nr::GETCWD;
    Statfs     => STATFS     = 137, nr::STATFS;
    Fstatfs    => FSTATFS    = 138, nr::FSTATFS;
    /// x86_64 332, asm-generic 291. Arch-neutral wire struct (`struct statx` is
    /// 256 bytes with the same offsets on both), so unlike `fstat` it needs no
    /// converter — only a number. The amd64 kernel had no arm for it until 4b
    /// batch 3b; every `statx` returned `ENOSYS`, which a modern `stat(1)` and
    /// Rust `std::fs::metadata` both try before `newfstatat`.
    Statx      => STATX      = 332, nr::STATX;

    // ── paths: the `*at` family only ───────────────────────────────────────
    // The legacy non-`at` spellings are x86-only and stay as shims in the amd64
    // kernel; see the crate header, rule 2.
    Openat     => OPENAT     = 257, nr::OPENAT;
    Mkdirat    => MKDIRAT    = 258, nr::MKDIRAT;
    Newfstatat => NEWFSTATAT = 262, nr::NEWFSTATAT;
    Unlinkat   => UNLINKAT   = 263, nr::UNLINKAT;
    Renameat   => RENAMEAT   = 264, nr::RENAMEAT;
    Symlinkat  => SYMLINKAT  = 266, nr::SYMLINKAT;
    Readlinkat => READLINKAT = 267, nr::READLINKAT;
    Faccessat  => FACCESSAT  = 269, nr::FACCESSAT;
    Utimensat  => UTIMENSAT  = 280, nr::UTIMENSAT;

    // ── readiness ──────────────────────────────────────────────────────────
    /// asm-generic has no `poll`(7) or `select`(23) — `ppoll` and `pselect6`
    /// are the only spellings — so the amd64 kernel's `7`/`23` arms are shims
    /// and this is the neutral name they narrow to.
    Ppoll      => PPOLL      = 271, nr::PPOLL;
    /// The `select` half of the same pair. Legitimately two-numbered — x86_64
    /// 270, asm-generic 72 — unlike `select`(23), which is x86-only and stays a
    /// shim.
    ///
    /// Nothing in the tree issues it on x86_64: musl's `select()`
    /// (`src/select/select.c`) compiles its `#ifdef SYS_select` branch on an
    /// architecture that has number 23, and only falls through to `pselect6` on
    /// one that does not — which is aarch64. Dispatched anyway because a program
    /// that calls it by hand got `ENOSYS` from a kernel that implements the call.
    Pselect6   => PSELECT6   = 270, nr::PSELECT6;

    // ── memory ─────────────────────────────────────────────────────────────
    Mmap       => MMAP       = 9,   nr::MMAP;
    Mprotect   => MPROTECT   = 10,  nr::MPROTECT;
    Munmap     => MUNMAP     = 11,  nr::MUNMAP;
    Brk        => BRK        = 12,  nr::BRK;
    /// Added 2026-09-07 with [`Self::Pread64`]. The amd64 kernel answers the
    /// advice decode out of `akuma_syscalls_mem::madvise::action`, which is
    /// where `MADV_FREE`'s deliberate `EINVAL` lives.
    Madvise    => MADVISE    = 28,  nr::MADVISE;
    /// Added 2026-09-07. `akuma_syscalls_mem::mremap` holds the
    /// move-vs-expand decision both kernels build against.
    Mremap     => MREMAP     = 25,  nr::MREMAP;

    // ── process lifecycle ──────────────────────────────────────────────────
    /// x86-only `fork`(57)/`vfork`(58) narrow to this with `CLONE_VM` clear;
    /// asm-generic has no number for either.
    Clone      => CLONE      = 56,  nr::CLONE;
    Execve     => EXECVE     = 59,  nr::EXECVE;
    Exit       => EXIT       = 60,  nr::EXIT;
    ExitGroup  => EXIT_GROUP = 231, nr::EXIT_GROUP;
    Wait4      => WAIT4      = 61,  nr::WAIT4;
    SchedYield => SCHED_YIELD = 24, nr::SCHED_YIELD;
    /// x86_64 202 is `futex` and asm-generic 202 is `accept` — the crossing
    /// that would turn a park into a socket accept.
    Futex      => FUTEX      = 202, nr::FUTEX;
    SetTidAddress => SET_TID_ADDRESS = 218, nr::SET_TID_ADDRESS;
    SetRobustList => SET_ROBUST_LIST = 273, nr::SET_ROBUST_LIST;
    Prlimit64  => PRLIMIT64  = 302, nr::PRLIMIT64;

    // ── identity ───────────────────────────────────────────────────────────
    // What these *return* on the amd64 target is a separate question from what
    // they are called: `getpid` answers a literal 1 and the uid/gid family
    // answers 0 there. Those are pinned decisions in `amd64/src/usermode.rs`,
    // not properties of this table.
    Getpid     => GETPID     = 39,  nr::GETPID;
    Getppid    => GETPPID    = 110, nr::GETPPID;
    /// x86_64 186 is `gettid`; asm-generic 178. Rust's `std` prints it in a
    /// panic message, which is how its absence announced itself on amd64.
    Gettid     => GETTID     = 186, nr::GETTID;
    Getuid     => GETUID     = 102, nr::GETUID;
    Getgid     => GETGID     = 104, nr::GETGID;
    Geteuid    => GETEUID    = 107, nr::GETEUID;
    Getegid    => GETEGID    = 108, nr::GETEGID;
    Setuid     => SETUID     = 105, nr::SETUID;
    Setgid     => SETGID     = 106, nr::SETGID;
    /// x86_64 115 is `getgroups`; asm-generic 158 — where x86_64 158 is
    /// `arch_prctl`, so this row is one of the pairs that makes the two-number
    /// shape earn itself. Added 2026-09-07 because `busybox id` on amd64 printed
    /// `uid=0 gid=0` and then `id: can't get groups`: the number had no variant,
    /// so it could not reach the `sys_getgroups` glue has had all along.
    Getgroups  => GETGROUPS  = 115, nr::GETGROUPS;
    Setpgid    => SETPGID    = 109, nr::SETPGID;
    Getpgid    => GETPGID    = 121, nr::GETPGID;
    Setsid     => SETSID     = 112, nr::SETSID;
    Getsid     => GETSID     = 124, nr::GETSID;

    // ── signals ────────────────────────────────────────────────────────────
    // Named here because both kernels dispatch the numbers. Every row below
    // has a **different** number on the two architectures, and three of the
    // crossings are live wrong answers rather than misses: x86_64 15
    // (`rt_sigreturn`) is asm-generic `nanosleep`, x86_64 128
    // (`rt_sigtimedwait`) is asm-generic `msgctl`, and x86_64 130
    // (`rt_sigsuspend`) is asm-generic `tkill`. A signal table is exactly where
    // a transposed number is least visible, because the caller is a libc
    // start-up path that ignores the result.
    RtSigaction   => RT_SIGACTION   = 13, nr::RT_SIGACTION;
    RtSigprocmask => RT_SIGPROCMASK = 14, nr::RT_SIGPROCMASK;
    /// x86_64 15, asm-generic 139. The amd64 kernel serves it locally (the
    /// register file it restores is `UserCtx`, not a `UserTrapFrame`); glue's
    /// row is `=> 0`, because on AArch64 `rt_sigreturn` is consumed inside the
    /// EL0 sync handler and never reaches the dispatcher.
    RtSigreturn   => RT_SIGRETURN   = 15, nr::RT_SIGRETURN;
    /// x86_64 62, asm-generic 129 — and `nr::KILL` is **Akuma's private 302**,
    /// not this. Naming the wrong constant here would dispatch `kill(2)` into
    /// the box-kill syscall.
    Kill          => KILL           = 62, nr::KILL_LINUX;
    /// x86_64 131, asm-generic 132.
    Sigaltstack   => SIGALTSTACK    = 131, nr::SIGALTSTACK;
    /// x86_64 200, asm-generic 130.
    Tkill         => TKILL          = 200, nr::TKILL;
    /// x86_64 234, asm-generic 131. Note the two numbers are each the *other*
    /// architecture's number for a different signal call — 131 is `sigaltstack`
    /// on x86_64 and `tgkill` on asm-generic, 200 is `tkill` on x86_64 and
    /// `mount` on asm-generic.
    Tgkill        => TGKILL         = 234, nr::TGKILL;

    // ── sockets ────────────────────────────────────────────────────────────
    Socket     => SOCKET     = 41,  nr::SOCKET;
    Bind       => BIND       = 49,  nr::BIND;
    Listen     => LISTEN     = 50,  nr::LISTEN;
    Accept     => ACCEPT     = 43,  nr::ACCEPT;
    Connect    => CONNECT    = 42,  nr::CONNECT;
    Sendto     => SENDTO     = 44,  nr::SENDTO;
    Recvfrom   => RECVFROM   = 45,  nr::RECVFROM;
    Sendmsg    => SENDMSG    = 46,  nr::SENDMSG;
    Recvmsg    => RECVMSG    = 47,  nr::RECVMSG;
    Setsockopt => SETSOCKOPT = 54,  nr::SETSOCKOPT;
    Getsockopt => GETSOCKOPT = 55,  nr::GETSOCKOPT;
    Shutdown   => SHUTDOWN   = 48,  nr::SHUTDOWN;

    // ── time ───────────────────────────────────────────────────────────────
    // `gettimeofday`(96), `settimeofday`(164) and `time`(201) are x86-only and
    // are absent by rule 2.
    // `alarm`(37) is x86-only too, and is the one absence with a consequence
    // worth naming: musl spells `alarm(3)` as `SYS_alarm` where the number
    // exists and as `setitimer` where it does not, so the aarch64 kernel
    // serves it through `Setitimer` below and this one needs a shim. See
    // `akuma_syscalls_glue::sys_alarm`.
    Nanosleep      => NANOSLEEP       = 35,  nr::NANOSLEEP;
    ClockGettime   => CLOCK_GETTIME   = 228, nr::CLOCK_GETTIME;
    ClockSettime   => CLOCK_SETTIME   = 227, nr::CLOCK_SETTIME;
    Adjtimex       => ADJTIMEX        = 159, nr::ADJTIMEX;
    // Added with C3 (2026-09-12). Every one of these had a working
    // implementation in `akuma-syscalls-time` and no number to reach it by on
    // this architecture, so each was an `ENOSYS` the other kernel does not
    // have. `clock_adjtime` is the widest crossing in the block — x86_64 305
    // against asm-generic 266 — and 305 is `akuma_syscalls_linux::nr::TIME`,
    // an Akuma-private number, which is exactly the wrong-arm-not-no-arm
    // failure this table exists to make impossible.
    ClockGetres    => CLOCK_GETRES    = 229, nr::CLOCK_GETRES;
    ClockNanosleep => CLOCK_NANOSLEEP = 230, nr::CLOCK_NANOSLEEP;
    ClockAdjtime   => CLOCK_ADJTIME   = 305, nr::CLOCK_ADJTIME;
    Setitimer      => SETITIMER       = 38,  nr::SETITIMER;
    Times          => TIMES           = 100, nr::TIMES;
    Getrusage      => GETRUSAGE       = 98,  nr::GETRUSAGE;

    // ── machine / misc ─────────────────────────────────────────────────────
    /// x86_64 63 is `uname` and asm-generic 63 is `read`. Of every crossing in
    /// this table this is the one that would be loudest and least explicable:
    /// a `uname` writing a `utsname` where a `read` was asked for.
    Uname      => UNAME      = 63,  nr::UNAME;
    Sysinfo    => SYSINFO    = 99,  nr::SYSINFO;
    Syslog     => SYSLOG     = 103, nr::SYSLOG;
    Reboot     => REBOOT     = 169, nr::REBOOT;
    Getrandom  => GETRANDOM  = 318, nr::GETRANDOM;
}

/// `openat(2)`'s flag word, **on which architecture** — the second vocabulary
/// this crate translates, and the same failure mode as the first.
///
/// # The problem
///
/// [`Syscall`] above exists because a syscall *number* means different things on
/// the two architectures. So does an `open(2)` *flag bit*, and for a reason that
/// is easy to miss: aarch64 Linux keeps the **32-bit ARM** fcntl values rather
/// than the asm-generic ones x86-64 uses, and the difference is not a shift or
/// an offset — it is a **permutation of four bits**, so every one of them is a
/// valid flag on both sides and none of them is ever rejected.
///
/// Taken from the musl headers this tree's own userspace is built against
/// (`userspace/tcc/vendor/musl-dev-{aarch64,x86_64}.apk`, `bits/fcntl.h`),
/// which is the right source for this question: what matters is not what a
/// kernel header says but what the libc in the guest actually passes.
///
/// | bit | aarch64 | x86_64 |
/// |---:|---|---|
/// | `0o40000`  | `O_DIRECTORY` | `O_DIRECT` |
/// | `0o100000` | `O_NOFOLLOW`  | `O_LARGEFILE` |
/// | `0o200000` | `O_DIRECT`    | `O_DIRECTORY` |
/// | `0o400000` | `O_LARGEFILE` | `O_NOFOLLOW` |
///
/// **Every other `O_*` bit is identical on the two architectures** —
/// `O_CREAT`, `O_EXCL`, `O_NOCTTY`, `O_TRUNC`, `O_APPEND`, `O_NONBLOCK`,
/// `O_DSYNC`, `O_ASYNC`, `O_NOATIME`, `O_CLOEXEC`, `O_PATH` and `__O_TMPFILE`
/// — which is exactly what makes this dangerous. A reader who spot-checks
/// `O_CREAT` and `O_CLOEXEC` concludes the encodings agree, and
/// `amd64/src/fd.rs` said so in a comment for months
/// ("on x86_64 they happen to share the same numeric encoding").
///
/// # Why it is a *silent* wrong answer, twice over
///
/// The permutation is two transpositions, and both of them turn one real flag
/// into another real flag:
///
/// - **`O_DIRECTORY` ↔ `O_DIRECT`.** An x86_64 caller asking for
///   `O_DIRECTORY` (`0o200000`) read with the aarch64 table is asking for
///   `O_DIRECT` — a cache hint — so the "this had better be a directory"
///   check never runs. In the other direction an `O_DIRECT` read becomes
///   `O_DIRECTORY`, and an ordinary file open is refused as not-a-directory.
/// - **`O_NOFOLLOW` ↔ `O_LARGEFILE`.** musl passes `O_LARGEFILE` on nothing
///   and glibc passes it on almost everything, so this one reads as
///   "don't follow symlinks" on a caller that never asked.
///
/// And the compound flag inherits it: `O_TMPFILE` is
/// `__O_TMPFILE | O_DIRECTORY`, so it is `0o20040000` on aarch64 and
/// `0o20200000` on x86_64. `akuma-syscalls-glue`'s `sys_openat` **refuses**
/// `O_TMPFILE` on purpose — apk-tools 3 probes for it, and an open that
/// succeeds and then silently discards the bytes surfaced as
/// `UNTRUSTED signature` over a download that was fine
/// (`docs/archive/APK_OTMPFILE_DIR_FD.md`). Fed an untranslated x86_64 flag
/// word, that refusal **does not fire**: `0o20200000 & 0o20040000` is
/// `0o20000000`, not the mask, so the guard tests false and the bug comes
/// back — on the architecture that has never seen it.
///
/// # Where to apply it
///
/// Once, at the amd64 syscall boundary, exactly like [`Syscall::from_x86_64`]:
/// what a `KernelFile` stores and what `akuma-syscalls-glue` reads is then the
/// asm-generic encoding on both kernels, and no shared crate has to know which
/// architecture it is serving.
pub mod open_flags {
    /// The x86_64 (asm-generic) values of the four permuted bits.
    ///
    /// Literals, because this crate owns the x86_64 table — the same split as
    /// the syscall numbers above.
    pub mod x86_64 {
        pub const O_DIRECT: u32 = 0o40_000;
        pub const O_LARGEFILE: u32 = 0o100_000;
        pub const O_DIRECTORY: u32 = 0o200_000;
        pub const O_NOFOLLOW: u32 = 0o400_000;
        /// `__O_TMPFILE | O_DIRECTORY`, x86_64 encoding.
        pub const O_TMPFILE: u32 = 0o20_200_000;
    }

    /// The aarch64 (32-bit ARM) values of the same four bits.
    ///
    /// `O_DIRECTORY` and `O_NOFOLLOW` are *paths into* `akuma-syscalls-linux`,
    /// which owns the aarch64 table, so those two cannot drift. `O_DIRECT` and
    /// `O_LARGEFILE` are literals because that crate does not name them: its
    /// rule is that a constant appears when a caller needs it, and nothing in
    /// the AArch64 kernel reads either flag. They are needed **here** even so —
    /// a translation that passed them through unchanged would leave them
    /// meaning the other flag, which is the whole defect.
    pub mod aarch64 {
        pub use akuma_syscalls_linux::flags::open::{O_DIRECTORY, O_NOFOLLOW, O_TMPFILE};
        pub const O_DIRECT: u32 = 0o200_000;
        pub const O_LARGEFILE: u32 = 0o400_000;
    }

    /// The four bits that differ. Everything outside this mask is shared.
    const PERMUTED: u32 = 0o40_000 | 0o100_000 | 0o200_000 | 0o400_000;

    /// Re-encode an x86_64 `open(2)` flag word in the aarch64 encoding — the
    /// one every shared crate in this tree reads.
    #[must_use]
    pub const fn x86_64_to_aarch64(flags: u32) -> u32 {
        let mut out = flags & !PERMUTED;
        if flags & x86_64::O_DIRECT != 0 {
            out |= aarch64::O_DIRECT;
        }
        if flags & x86_64::O_LARGEFILE != 0 {
            out |= aarch64::O_LARGEFILE;
        }
        if flags & x86_64::O_DIRECTORY != 0 {
            out |= aarch64::O_DIRECTORY;
        }
        if flags & x86_64::O_NOFOLLOW != 0 {
            out |= aarch64::O_NOFOLLOW;
        }
        out
    }

    /// The inverse, for a value going back out to an x86_64 caller —
    /// `fcntl(F_GETFL)` is the one that matters.
    ///
    /// Written out rather than aliased to [`x86_64_to_aarch64`]. The two
    /// happen to be the same function today, because a permutation made of
    /// transpositions is its own inverse, and `translation_is_an_involution`
    /// pins that — but leaning on it silently would mean a future bit that is
    /// *not* self-inverse (a genuine renumbering rather than a swap) breaking
    /// one direction with nothing to say which.
    #[must_use]
    pub const fn aarch64_to_x86_64(flags: u32) -> u32 {
        let mut out = flags & !PERMUTED;
        if flags & aarch64::O_DIRECT != 0 {
            out |= x86_64::O_DIRECT;
        }
        if flags & aarch64::O_LARGEFILE != 0 {
            out |= x86_64::O_LARGEFILE;
        }
        if flags & aarch64::O_DIRECTORY != 0 {
            out |= x86_64::O_DIRECTORY;
        }
        if flags & aarch64::O_NOFOLLOW != 0 {
            out |= x86_64::O_NOFOLLOW;
        }
        out
    }
}

/// `struct stat`, on which architecture.
///
/// The third vocabulary this crate carries, after the syscall numbers and
/// `open(2)`'s flag word. `akuma_syscalls_linux::Stat` is the **aarch64**
/// (`asm-generic/stat.h`) layout — its own header says so, and every shared
/// crate that fills a `stat` fills that one. x86_64 predates `asm-generic` here
/// too: `struct stat` is 144 bytes rather than 128, `st_nlink` is 8 bytes at
/// offset 16 rather than 4 at offset 20, and `st_mode` lands at 24 rather than
/// 16 — so a buffer filled in the aarch64 layout and handed to an x86_64 `ls`
/// reads `st_mode` out of the middle of `st_nlink` and every field after it is
/// shifted.
///
/// The amd64 kernel used to carry this as a hand-rolled `encode_stat` writing
/// literal offsets into a `[u8; 144]`, beside a comment calling it "proposal
/// item 5 territory". This is that item: the layout with `offset_of!`
/// assertions, and a converter from the shared fill.
pub mod stat {
    use akuma_syscalls_linux::Stat as AsmGeneric;

    /// `struct stat` as x86_64 Linux defines it
    /// (`arch/x86/include/uapi/asm/stat.h`, 64-bit), 144 bytes.
    ///
    /// The `__pad0`/`__unused` fields are `pub` so the struct reads next to the
    /// C one — the same choice `akuma_syscalls_linux::Stat` makes.
    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug)]
    #[allow(clippy::pub_underscore_fields)]
    pub struct X8664 {
        pub st_dev: u64,
        pub st_ino: u64,
        pub st_nlink: u64,
        pub st_mode: u32,
        pub st_uid: u32,
        pub st_gid: u32,
        pub __pad0: u32,
        pub st_rdev: u64,
        pub st_size: i64,
        pub st_blksize: i64,
        pub st_blocks: i64,
        pub st_atime: i64,
        pub st_atime_nsec: i64,
        pub st_mtime: i64,
        pub st_mtime_nsec: i64,
        pub st_ctime: i64,
        pub st_ctime_nsec: i64,
        pub __unused: [i64; 3],
    }

    /// Re-lay a shared (aarch64) `struct stat` in the x86_64 layout.
    ///
    /// Field-by-field, not a reinterpret: the two structs disagree about
    /// `st_nlink`'s width and about the order of `st_rdev`/`st_mode`, so there
    /// is no cast that does this. Every field `akuma-syscalls-glue` actually
    /// fills is carried; the padding words stay zero, matching what the old
    /// `encode_stat` left them as.
    #[must_use]
    pub fn to_x86_64(s: &AsmGeneric) -> X8664 {
        X8664 {
            st_dev: s.st_dev,
            st_ino: s.st_ino,
            st_nlink: u64::from(s.st_nlink),
            st_mode: s.st_mode,
            st_uid: s.st_uid,
            st_gid: s.st_gid,
            __pad0: 0,
            st_rdev: s.st_rdev,
            st_size: s.st_size,
            st_blksize: i64::from(s.st_blksize),
            st_blocks: s.st_blocks,
            st_atime: s.st_atime,
            st_atime_nsec: s.st_atime_nsec,
            st_mtime: s.st_mtime,
            st_mtime_nsec: s.st_mtime_nsec,
            st_ctime: s.st_ctime,
            st_ctime_nsec: s.st_ctime_nsec,
            __unused: [0; 3],
        }
    }

    // The offsets the amd64 kernel's `encode_stat` spelled as literals
    // (`ST_INO`, `ST_NLINK`, `ST_MODE`, `ST_SIZE`, `ST_BLKSIZE`, `ST_BLOCKS`,
    // `ST_ATIME`, `ST_MTIME`, `ST_CTIME`). A layout change that moves one is a
    // build failure rather than an `ls` reading the wrong field.
    const _: () = assert!(core::mem::size_of::<X8664>() == 144);
    const _: () = assert!(core::mem::offset_of!(X8664, st_ino) == 8);
    const _: () = assert!(core::mem::offset_of!(X8664, st_nlink) == 16);
    const _: () = assert!(core::mem::offset_of!(X8664, st_mode) == 24);
    const _: () = assert!(core::mem::offset_of!(X8664, st_rdev) == 40);
    const _: () = assert!(core::mem::offset_of!(X8664, st_size) == 48);
    const _: () = assert!(core::mem::offset_of!(X8664, st_blksize) == 56);
    const _: () = assert!(core::mem::offset_of!(X8664, st_blocks) == 64);
    const _: () = assert!(core::mem::offset_of!(X8664, st_atime) == 72);
    const _: () = assert!(core::mem::offset_of!(X8664, st_mtime) == 88);
    const _: () = assert!(core::mem::offset_of!(X8664, st_ctime) == 104);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant the table names must survive a round trip on both
    /// architectures.
    ///
    /// `ALL` is generated from the same rows as the encodes and decodes, so this
    /// is exhaustive by construction — unlike the hand-written list it replaced,
    /// which could go stale silently.
    #[test]
    fn round_trip_on_both_architectures() {
        for &s in Syscall::ALL {
            let (x, a) = (s.to_x86_64(), s.to_aarch64());
            assert_eq!(Syscall::from_x86_64(x), Some(s), "x86_64 {x} -> {s:?}");
            assert_eq!(Syscall::from_aarch64(a), Some(s), "aarch64 {a} -> {s:?}");
        }
    }

    /// No two syscalls may share a number on either side.
    ///
    /// The `match` arms already make a duplicate an `unreachable_patterns`
    /// warning, but a warning is not a gate — and `to_*` would still happily
    /// encode both variants to the same number, so the round trip above would
    /// fail somewhere unhelpful. This says which two rows collided.
    #[test]
    fn every_number_is_claimed_once() {
        for (i, &a) in Syscall::ALL.iter().enumerate() {
            for &b in &Syscall::ALL[i + 1..] {
                assert_ne!(a.to_x86_64(), b.to_x86_64(), "x86_64: {a:?} vs {b:?}");
                assert_ne!(a.to_aarch64(), b.to_aarch64(), "aarch64: {a:?} vs {b:?}");
            }
        }
    }

    /// The two tables must actually differ, and differ *where Linux differs*.
    ///
    /// Modelled on `akuma-firecracker`'s `no_address_is_hardcoded`, and for the
    /// same reason: the failure this guards against is a second table that was
    /// copied from the first, which looks correct until it is used. If this ever
    /// passes trivially — because someone "unified" the numbering — the bug it
    /// exists to catch is already in the tree.
    #[test]
    fn tables_disagree_where_linux_does() {
        let differing = Syscall::ALL
            .iter()
            .filter(|s| s.to_x86_64() != s.to_aarch64())
            .count();
        assert_eq!(
            differing,
            Syscall::ALL.len(),
            "the two ABIs agree on some syscall, which Linux does not"
        );

        // Spot-checks against the kernel tables, so a mass edit cannot drift
        // both sides together.
        assert_eq!(Syscall::Read.to_x86_64(), 0);
        assert_eq!(Syscall::Read.to_aarch64(), 63);
        assert_eq!(Syscall::Exit.to_x86_64(), 60);
        assert_eq!(Syscall::Exit.to_aarch64(), 93);
        assert_eq!(Syscall::ExitGroup.to_x86_64(), 231);
        assert_eq!(Syscall::ExitGroup.to_aarch64(), 94);
    }

    /// The collision that makes using the wrong table *worse* than an error.
    ///
    /// Number 0 is `read` on x86_64 and `io_setup` under `asm-generic`. A
    /// dispatcher fed the wrong table does not fail to find a handler — it finds
    /// the wrong one, and a `read` that runs `io_setup` corrupts rather than
    /// crashes.
    #[test]
    fn zero_means_different_things() {
        assert_eq!(Syscall::from_x86_64(0), Some(Syscall::Read));
        assert_eq!(nr::IO_SETUP, 0);
        assert_ne!(Syscall::from_aarch64(0), Some(Syscall::Read));
    }

    /// The crossings that C1 exists to make impossible, spelled out.
    ///
    /// Each of these numbers is dispatched by the amd64 kernel meaning one call
    /// and by `akuma-syscalls-glue` meaning another. They are the reason the
    /// translation happens at the boundary in one place rather than being
    /// assumed to be unnecessary anywhere.
    #[test]
    fn collisions_that_would_corrupt() {
        // (number, what x86_64 means, what asm-generic means)
        let crossings = [
            (0u64, Syscall::Read, Syscall::from_aarch64(0)),
            (32, Syscall::Dup, Some(Syscall::Flock)),
            (43, Syscall::Accept, Some(Syscall::Statfs)),
            (63, Syscall::Uname, Some(Syscall::Read)),
            (73, Syscall::Flock, Some(Syscall::Ppoll)),
            (79, Syscall::Getcwd, Some(Syscall::Newfstatat)),
            (202, Syscall::Futex, Some(Syscall::Accept)),
        ];
        for (n, x, a) in crossings {
            assert_eq!(Syscall::from_x86_64(n), Some(x), "x86_64 {n}");
            assert_eq!(Syscall::from_aarch64(n), a, "aarch64 {n}");
            assert_ne!(Some(x), a, "{n} would not be a crossing at all");
        }
    }

    #[test]
    fn unknown_numbers_decode_to_none() {
        assert_eq!(Syscall::from_x86_64(u64::MAX), None);
        assert_eq!(Syscall::from_aarch64(u64::MAX), None);
        // 231 is exit_group on x86_64 and unassigned under asm-generic.
        assert_eq!(Syscall::from_x86_64(231), Some(Syscall::ExitGroup));
        assert_eq!(Syscall::from_aarch64(231), None);
    }

    /// The x86-only legacy spellings must stay *out* of the table.
    ///
    /// Rule 2 of the crate header. Each of these has no asm-generic number, so a
    /// row for it would have to invent one; the amd64 kernel narrows each to an
    /// `*at` call with `AT_FDCWD` instead. If one of these ever decodes, someone
    /// has given a legacy call a number it does not have on aarch64.
    #[test]
    fn x86_only_legacy_spellings_are_not_in_the_table() {
        // `open`, `stat`, `lstat`, `poll`, `access`, `pipe`, `select`, `dup2`,
        // `fork`, `vfork`, `rename`, `mkdir`, `rmdir`, `unlink`, `symlink`,
        // `readlink`, `gettimeofday`, `getpgrp`, `arch_prctl`, `settimeofday`,
        // `time`.
        for n in [2u64, 4, 6, 7, 21, 22, 23, 33, 57, 58, 82, 83, 84, 87, 88, 89, 96, 111, 158, 164, 201]
        {
            assert_eq!(
                Syscall::from_x86_64(n),
                None,
                "x86_64 {n} is a legacy spelling with no asm-generic twin"
            );
        }
    }

    /// The two numbers this widening had to add to `akuma-syscalls-linux`.
    ///
    /// Both are calls the AArch64 kernel does not dispatch, so nothing else in
    /// the tree would notice a wrong value.
    #[test]
    fn newly_named_asm_generic_numbers() {
        assert_eq!(Syscall::Syslog.to_aarch64(), 116);
        assert_eq!(Syscall::Getsid.to_aarch64(), 156);
    }

    // ── the open(2) flag vocabulary ──────────────────────────────────────

    /// The four permuted bits round trip, and the translation is total.
    ///
    /// `PERMUTED` is private, so this walks every bit of the word instead:
    /// a translation that dropped a bit, or invented one, fails here.
    #[test]
    fn every_bit_survives_a_round_trip() {
        for bit in 0..32 {
            let x = 1u32 << bit;
            let a = open_flags::x86_64_to_aarch64(x);
            assert_eq!(a.count_ones(), 1, "x86_64 bit {bit} did not map to one bit");
            assert_eq!(
                open_flags::aarch64_to_x86_64(a),
                x,
                "x86_64 bit {bit} did not round trip"
            );
        }
    }

    /// Only four bits move. This is the claim the module's table makes, and it
    /// is the one a reader is most likely to assume without checking — the
    /// comment in `amd64/src/fd.rs` asserted the *opposite* ("they happen to
    /// share the same numeric encoding") for months.
    #[test]
    fn exactly_four_bits_are_permuted() {
        let mut moved = [0u32; 32];
        let mut n = 0;
        for bit in 0..32u32 {
            let x = 1u32 << bit;
            if open_flags::x86_64_to_aarch64(x) != x {
                moved[n] = x;
                n += 1;
            }
        }
        assert_eq!(
            &moved[..n],
            &[0o40_000, 0o100_000, 0o200_000, 0o400_000],
            "the permuted set is not the four bits the table names"
        );
    }

    /// A permutation made of transpositions is its own inverse. Pinned rather
    /// than relied on — see [`open_flags::aarch64_to_x86_64`].
    #[test]
    fn translation_is_an_involution() {
        for bit in 0..32u32 {
            let x = 1u32 << bit;
            assert_eq!(
                open_flags::x86_64_to_aarch64(x),
                open_flags::aarch64_to_x86_64(x),
                "bit {bit}"
            );
        }
    }

    /// The aarch64 half must agree with the crate that owns the aarch64 table.
    ///
    /// Two of the four are re-exports and cannot drift; the other two are
    /// literals here, so this is the check that they were read off the same
    /// header as their neighbours.
    #[test]
    fn aarch64_values_match_the_asm_generic_crate() {
        use akuma_syscalls_linux::flags::open as linux;
        assert_eq!(open_flags::aarch64::O_DIRECTORY, linux::O_DIRECTORY);
        assert_eq!(open_flags::aarch64::O_NOFOLLOW, linux::O_NOFOLLOW);
        assert_eq!(open_flags::aarch64::O_TMPFILE, linux::O_TMPFILE);
        // The two this crate declares itself, against the ARM fcntl block they
        // came out of: `O_DIRECT` and `O_LARGEFILE` are the *other* halves of
        // the two transpositions, so each must equal the x86_64 value of its
        // partner.
        assert_eq!(open_flags::aarch64::O_DIRECT, open_flags::x86_64::O_DIRECTORY);
        assert_eq!(open_flags::aarch64::O_LARGEFILE, open_flags::x86_64::O_NOFOLLOW);
    }

    /// **The silent failure this module exists to prevent.**
    ///
    /// `akuma-syscalls-glue`'s `sys_openat` refuses `O_TMPFILE` on purpose:
    /// apk-tools 3 probes for it, and an open that succeeds and then discards
    /// the bytes surfaced as `UNTRUSTED signature` over a good download
    /// (`docs/archive/APK_OTMPFILE_DIR_FD.md`). Fed the x86_64 encoding
    /// untranslated, that guard tests false — so the assertion that matters is
    /// the *negative* one.
    #[test]
    fn untranslated_o_tmpfile_defeats_the_guard() {
        let asked = open_flags::x86_64::O_TMPFILE;
        let guard = akuma_syscalls_linux::flags::open::O_TMPFILE;

        // What the bug looks like: the flag is set, and the guard says no.
        assert_ne!(asked & guard, guard, "this is the defect, not the fix");

        // And what the translation restores.
        assert_eq!(
            open_flags::x86_64_to_aarch64(asked) & guard,
            guard,
            "translated, the refusal fires"
        );
    }

    /// The other transposition, which is worse than a missed refusal because it
    /// **invents** a flag the caller did not pass.
    ///
    /// An x86_64 `O_DIRECTORY` read with the aarch64 table is `O_DIRECT`, and
    /// an x86_64 `O_DIRECT` is `O_DIRECTORY` — so a plain file open acquires a
    /// "must be a directory" requirement it never asked for. Both directions
    /// are real flags, which is why nothing rejects either one.
    #[test]
    fn o_directory_and_o_direct_are_each_other() {
        use akuma_syscalls_linux::flags::open as linux;
        assert_eq!(open_flags::x86_64::O_DIRECTORY, open_flags::aarch64::O_DIRECT);
        assert_eq!(open_flags::x86_64::O_DIRECT, linux::O_DIRECTORY);
        assert_eq!(
            open_flags::x86_64_to_aarch64(open_flags::x86_64::O_DIRECTORY),
            linux::O_DIRECTORY
        );
    }

    /// The bits a spot-check would look at, which is why the permutation hides.
    #[test]
    fn the_common_bits_really_are_common() {
        use akuma_syscalls_linux::flags::open as linux;
        for f in [
            linux::O_CREAT,
            linux::O_EXCL,
            linux::O_NOCTTY,
            linux::O_TRUNC,
            linux::O_APPEND,
            linux::O_NONBLOCK,
            linux::O_CLOEXEC,
            linux::O_PATH,
            linux::O_ACCMODE,
        ] {
            assert_eq!(open_flags::x86_64_to_aarch64(f), f, "0o{f:o} should not move");
        }
    }

    /// The fields an x86_64 `ls -l` and `apk` read must land where the kernel's
    /// old hand-rolled `encode_stat` wrote them, and the converter must carry
    /// each one across the width change on `st_nlink`.
    #[test]
    fn stat_x86_64_layout_and_conversion() {
        use akuma_syscalls_linux::Stat as G;
        let g = G {
            st_dev: 0,
            st_ino: 42,
            st_mode: 0o100_644,
            st_nlink: 3,
            st_size: 123_456,
            st_blksize: 4096,
            st_blocks: 241,
            st_atime: 111,
            st_mtime: 222,
            st_ctime: 333,
            st_rdev: 0,
            ..Default::default()
        };
        let x = stat::to_x86_64(&g);
        assert_eq!(x.st_ino, 42);
        assert_eq!(x.st_nlink, 3);
        assert_eq!(core::mem::size_of_val(&x.st_nlink), 8);
        assert_eq!(x.st_mode, 0o100_644);
        assert_eq!(x.st_size, 123_456);
        assert_eq!(x.st_blocks, 241);
        assert_eq!(x.st_mtime, 222);
        // Offset *and* width on the field a 64-bit `ls` reads for the size.
        assert_eq!(core::mem::offset_of!(stat::X8664, st_size), 48);
        assert_eq!(core::mem::size_of_val(&x.st_size), 8);
    }
}

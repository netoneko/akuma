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
    Setpgid    => SETPGID    = 109, nr::SETPGID;
    Getpgid    => GETPGID    = 121, nr::GETPGID;
    Setsid     => SETSID     = 112, nr::SETSID;
    Getsid     => GETSID     = 124, nr::GETSID;

    // ── signals ────────────────────────────────────────────────────────────
    // Named here because both kernels dispatch the numbers. The amd64 kernel
    // has no delivery at all (A2); that divergence lives at its arms.
    RtSigaction   => RT_SIGACTION   = 13, nr::RT_SIGACTION;
    RtSigprocmask => RT_SIGPROCMASK = 14, nr::RT_SIGPROCMASK;

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
    Nanosleep     => NANOSLEEP      = 35,  nr::NANOSLEEP;
    ClockGettime  => CLOCK_GETTIME  = 228, nr::CLOCK_GETTIME;
    ClockSettime  => CLOCK_SETTIME  = 227, nr::CLOCK_SETTIME;
    Adjtimex      => ADJTIMEX       = 159, nr::ADJTIMEX;

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
}

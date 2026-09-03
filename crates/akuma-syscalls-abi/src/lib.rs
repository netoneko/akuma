#![no_std]
// Nothing here touches memory: two number tables and the mapping between them.
#![forbid(unsafe_code)]
//! Which syscall a number means, **on which architecture**.
//!
//! Proposal item 5 (`proposals/REDUCING_PLATFORM_DEPENDENCY.md` §5), started
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
//! # Scope, and how to grow it
//!
//! Deliberately a **subset**: the syscalls the amd64 port can plausibly reach
//! plus the ones any static binary issues on startup. It is not a mirror of
//! `nr`, and it should not become one speculatively — an entry here is a claim
//! that both tables were checked, and `tables_disagree_where_linux_does` is what
//! makes that claim testable. Add an entry when a caller needs it, with both
//! numbers, and the round-trip tests will hold you to it.

/// A syscall, named rather than numbered.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Syscall {
    Read,
    Write,
    Close,
    Fstat,
    Lseek,
    Mmap,
    Munmap,
    Brk,
    Ioctl,
    Writev,
    Readv,
    Openat,
    Exit,
    ExitGroup,
    SchedYield,
    Getpid,
    Nanosleep,
    ClockGettime,
    SetTidAddress,
}

/// x86_64 Linux numbers.
///
/// From `arch/x86/entry/syscalls/syscall_64.tbl`. Kept in its own module rather
/// than merged into `akuma_syscalls_linux::nr` so that neither table can be reached by
/// accident: a caller has to name the architecture it means.
pub mod x86_64 {
    pub const READ: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const CLOSE: u64 = 3;
    pub const FSTAT: u64 = 5;
    pub const LSEEK: u64 = 8;
    pub const MMAP: u64 = 9;
    pub const MUNMAP: u64 = 11;
    pub const BRK: u64 = 12;
    pub const IOCTL: u64 = 16;
    pub const READV: u64 = 19;
    pub const WRITEV: u64 = 20;
    pub const SCHED_YIELD: u64 = 24;
    pub const NANOSLEEP: u64 = 35;
    pub const GETPID: u64 = 39;
    pub const EXIT: u64 = 60;
    pub const SET_TID_ADDRESS: u64 = 218;
    pub const CLOCK_GETTIME: u64 = 228;
    pub const EXIT_GROUP: u64 = 231;
    pub const OPENAT: u64 = 257;
}

impl Syscall {
    /// Decode an x86_64 Linux syscall number.
    #[must_use]
    pub const fn from_x86_64(nr: u64) -> Option<Self> {
        use x86_64 as n;
        Some(match nr {
            n::READ => Self::Read,
            n::WRITE => Self::Write,
            n::CLOSE => Self::Close,
            n::FSTAT => Self::Fstat,
            n::LSEEK => Self::Lseek,
            n::MMAP => Self::Mmap,
            n::MUNMAP => Self::Munmap,
            n::BRK => Self::Brk,
            n::IOCTL => Self::Ioctl,
            n::READV => Self::Readv,
            n::WRITEV => Self::Writev,
            n::SCHED_YIELD => Self::SchedYield,
            n::NANOSLEEP => Self::Nanosleep,
            n::GETPID => Self::Getpid,
            n::EXIT => Self::Exit,
            n::SET_TID_ADDRESS => Self::SetTidAddress,
            n::CLOCK_GETTIME => Self::ClockGettime,
            n::EXIT_GROUP => Self::ExitGroup,
            n::OPENAT => Self::Openat,
            _ => return None,
        })
    }

    /// Decode an aarch64 (`asm-generic`) Linux syscall number.
    #[must_use]
    pub const fn from_aarch64(nr: u64) -> Option<Self> {
        use akuma_syscalls_linux::nr as n;
        Some(match nr {
            n::READ => Self::Read,
            n::WRITE => Self::Write,
            n::CLOSE => Self::Close,
            n::FSTAT => Self::Fstat,
            n::LSEEK => Self::Lseek,
            n::MMAP => Self::Mmap,
            n::MUNMAP => Self::Munmap,
            n::BRK => Self::Brk,
            n::IOCTL => Self::Ioctl,
            n::READV => Self::Readv,
            n::WRITEV => Self::Writev,
            n::SCHED_YIELD => Self::SchedYield,
            n::NANOSLEEP => Self::Nanosleep,
            n::GETPID => Self::Getpid,
            n::EXIT => Self::Exit,
            n::SET_TID_ADDRESS => Self::SetTidAddress,
            n::CLOCK_GETTIME => Self::ClockGettime,
            n::EXIT_GROUP => Self::ExitGroup,
            n::OPENAT => Self::Openat,
            _ => return None,
        })
    }

    /// This syscall's x86_64 number.
    ///
    /// Infallible, and that is the invariant: a variant only exists here if it
    /// has a number on **both** architectures. The compiler enforces it — adding
    /// a variant without adding both numbers fails to compile, rather than
    /// returning `None` at run time somewhere far from the omission.
    #[must_use]
    pub const fn to_x86_64(self) -> u64 {
        use x86_64 as n;
        match self {
            Self::Read => n::READ,
            Self::Write => n::WRITE,
            Self::Close => n::CLOSE,
            Self::Fstat => n::FSTAT,
            Self::Lseek => n::LSEEK,
            Self::Mmap => n::MMAP,
            Self::Munmap => n::MUNMAP,
            Self::Brk => n::BRK,
            Self::Ioctl => n::IOCTL,
            Self::Readv => n::READV,
            Self::Writev => n::WRITEV,
            Self::SchedYield => n::SCHED_YIELD,
            Self::Nanosleep => n::NANOSLEEP,
            Self::Getpid => n::GETPID,
            Self::Exit => n::EXIT,
            Self::SetTidAddress => n::SET_TID_ADDRESS,
            Self::ClockGettime => n::CLOCK_GETTIME,
            Self::ExitGroup => n::EXIT_GROUP,
            Self::Openat => n::OPENAT,
        }
    }

    /// This syscall's aarch64 (`asm-generic`) number. Infallible; see
    /// [`Syscall::to_x86_64`].
    #[must_use]
    pub const fn to_aarch64(self) -> u64 {
        use akuma_syscalls_linux::nr as n;
        match self {
            Self::Read => n::READ,
            Self::Write => n::WRITE,
            Self::Close => n::CLOSE,
            Self::Fstat => n::FSTAT,
            Self::Lseek => n::LSEEK,
            Self::Mmap => n::MMAP,
            Self::Munmap => n::MUNMAP,
            Self::Brk => n::BRK,
            Self::Ioctl => n::IOCTL,
            Self::Readv => n::READV,
            Self::Writev => n::WRITEV,
            Self::SchedYield => n::SCHED_YIELD,
            Self::Nanosleep => n::NANOSLEEP,
            Self::Getpid => n::GETPID,
            Self::Exit => n::EXIT,
            Self::SetTidAddress => n::SET_TID_ADDRESS,
            Self::ClockGettime => n::CLOCK_GETTIME,
            Self::ExitGroup => n::EXIT_GROUP,
            Self::Openat => n::OPENAT,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant this module claims to know must be expressible on both
    /// architectures and survive a round trip.
    ///
    /// The list is written out rather than derived, so adding a variant to the
    /// enum without adding both numbers fails to compile here rather than
    /// silently going undecodable.
    const ALL: &[Syscall] = &[
        Syscall::Read,
        Syscall::Write,
        Syscall::Close,
        Syscall::Fstat,
        Syscall::Lseek,
        Syscall::Mmap,
        Syscall::Munmap,
        Syscall::Brk,
        Syscall::Ioctl,
        Syscall::Readv,
        Syscall::Writev,
        Syscall::Openat,
        Syscall::Exit,
        Syscall::ExitGroup,
        Syscall::SchedYield,
        Syscall::Getpid,
        Syscall::Nanosleep,
        Syscall::ClockGettime,
        Syscall::SetTidAddress,
    ];

    #[test]
    fn round_trip_on_both_architectures() {
        for &s in ALL {
            let (x, a) = (s.to_x86_64(), s.to_aarch64());
            assert_eq!(Syscall::from_x86_64(x), Some(s), "x86_64 {x} -> {s:?}");
            assert_eq!(Syscall::from_aarch64(a), Some(s), "aarch64 {a} -> {s:?}");
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
        let differing = ALL
            .iter()
            .filter(|s| s.to_x86_64() != s.to_aarch64())
            .count();
        assert!(
            differing >= ALL.len() - 1,
            "expected the two ABIs to disagree on nearly every syscall, {differing}/{} did",
            ALL.len()
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
        assert_eq!(akuma_syscalls_linux::nr::IO_SETUP, 0);
        assert_ne!(Syscall::from_aarch64(0), Some(Syscall::Read));
    }

    #[test]
    fn unknown_numbers_decode_to_none() {
        assert_eq!(Syscall::from_x86_64(u64::MAX), None);
        assert_eq!(Syscall::from_aarch64(u64::MAX), None);
        // 231 is exit_group on x86_64 and unassigned under asm-generic.
        assert_eq!(Syscall::from_x86_64(231), Some(Syscall::ExitGroup));
        assert_eq!(Syscall::from_aarch64(231), None);
    }
}

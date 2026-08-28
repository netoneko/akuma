//! The flag and bitfield tables: `O_*`, `AT_*`, `F_*`, `MAP_*`, `PROT_*`,
//! `MADV_*`, `MREMAP_*`, `EPOLL*` and `POLL*`.
//!
//! Every one of these was a function-local `const` in the syscall layer before
//! 2026-08-27 — 35 of them across `fs.rs`, `mem.rs`, `net.rs`, `poll.rs`,
//! `proc.rs` and `pidfd.rs`, several defined more than once and two of them
//! (`O_NONBLOCK`, `O_CLOEXEC`) at two different widths in two files.
//!
//! # The arm64 `O_*` values are not the asm-generic ones
//!
//! This is the trap in the whole file. aarch64 Linux keeps the **32-bit ARM**
//! fcntl values, not the asm-generic ones x86-64 and riscv use. The difference
//! that matters here is `O_DIRECTORY`: `0o40000` on arm/arm64, `0o200000`
//! elsewhere. musl, glibc and Go all pass the arm64 encoding on this target, so
//! that is what these constants are — see [`open::O_TMPFILE`], whose value is
//! `__O_TMPFILE | O_DIRECTORY` and therefore inherits the same split.

/// `openat(2)` flags, arm64 encoding.
///
/// The canonical home for these. `akuma_exec::process::open_flags` re-exports
/// this module rather than declaring its own copy — it used to hold the
/// definition, which put the file-open ABI inside the process/exec crate and
/// left `src/syscall/fs.rs` and `src/syscall/pidfd.rs` redeclaring the two bits
/// it was missing.
pub mod open {
    pub const O_RDONLY: u32 = 0;
    pub const O_WRONLY: u32 = 1;
    pub const O_RDWR: u32 = 2;
    /// The low two bits, which are an enum rather than flags.
    pub const O_ACCMODE: u32 = 3;
    pub const O_CREAT: u32 = 0o100;
    pub const O_EXCL: u32 = 0o200;
    pub const O_NOCTTY: u32 = 0o400;
    pub const O_TRUNC: u32 = 0o1000;
    pub const O_APPEND: u32 = 0o2000;
    pub const O_NONBLOCK: u32 = 0o4000;
    /// **arm64 value.** `0o200000` on x86-64 — see the module docs.
    pub const O_DIRECTORY: u32 = 0o40000;
    pub const O_NOFOLLOW: u32 = 0o100000;
    pub const O_CLOEXEC: u32 = 0o2000000;
    pub const O_PATH: u32 = 0o10000000;
    /// `__O_TMPFILE | O_DIRECTORY`, in the **arm64** encoding.
    ///
    /// arm64 keeps the 32-bit ARM fcntl values (`O_DIRECTORY = 0o40000`),
    /// *not* the asm-generic ones x86/riscv use (`0o200000`); this is what
    /// musl, glibc and Go all pass on this target. The kernel does not
    /// implement tmpfiles;
    /// `sys_openat` rejects the flag so portable callers (apk-tools 3's atomic
    /// writes) take their `.tmp` + `renameat` fallback instead of writing into
    /// a directory fd.
    pub const O_TMPFILE: u32 = 0o20040000;
}

/// `*at(2)` `flags` bits and the `AT_FDCWD` sentinel.
pub mod at {
    /// `AT_FDCWD` as the raw `i32` a `dirfd` argument carries.
    pub const AT_FDCWD: i32 = -100;
    pub const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
    pub const AT_REMOVEDIR: u32 = 0x200;
    pub const AT_SYMLINK_FOLLOW: u32 = 0x400;
    pub const AT_NO_AUTOMOUNT: u32 = 0x800;
    pub const AT_EMPTY_PATH: u32 = 0x1000;
}

/// `utimensat(2)` sentinel values for `timespec.tv_nsec`.
///
/// They occupy the `tv_nsec` field, not a flags word, which is why they are
/// `i64`: `Timespec::tv_nsec` is signed on this ABI. Both are just under
/// `1 << 30`, so they can never collide with a real nanosecond value (< 1e9).
pub mod utimensat {
    /// Set this timestamp to the current time, ignoring `tv_sec`.
    pub const UTIME_NOW: i64 = (1 << 30) - 1;
    /// Leave this timestamp unchanged, ignoring `tv_sec`.
    pub const UTIME_OMIT: i64 = (1 << 30) - 2;
}

/// `fcntl(2)` commands.
pub mod fcntl {
    pub const F_DUPFD: u32 = 0;
    pub const F_GETFD: u32 = 1;
    pub const F_SETFD: u32 = 2;
    pub const F_GETFL: u32 = 3;
    pub const F_SETFL: u32 = 4;
    pub const F_GETLK: u32 = 5;
    pub const F_SETLK: u32 = 6;
    pub const F_SETLKW: u32 = 7;
    pub const F_SETOWN: u32 = 8;
    pub const F_GETOWN: u32 = 9;
    pub const F_DUPFD_CLOEXEC: u32 = 1030;
    /// The one `F_SETFD` bit that exists.
    pub const FD_CLOEXEC: u32 = 1;
}

/// `mmap(2)` `flags`. Values match Linux aarch64.
pub mod map {
    pub const MAP_SHARED: u32 = 0x01;
    pub const MAP_PRIVATE: u32 = 0x02;
    pub const MAP_FIXED: u32 = 0x10;
    pub const MAP_ANONYMOUS: u32 = 0x20;
    pub const MAP_NORESERVE: u32 = 0x4000;
    pub const MAP_POPULATE: u32 = 0x8000;
    /// Hint-only on Linux; ignored here.
    pub const MAP_STACK: u32 = 0x20000;
    pub const MAP_FIXED_NOREPLACE: u32 = 0x100000;
}

/// `mmap(2)`/`mprotect(2)` `prot`.
pub mod prot {
    pub const PROT_NONE: u32 = 0;
    pub const PROT_READ: u32 = 0x1;
    pub const PROT_WRITE: u32 = 0x2;
    pub const PROT_EXEC: u32 = 0x4;
}

/// `mremap(2)` `flags`.
pub mod mremap {
    pub const MREMAP_MAYMOVE: u32 = 1;
    pub const MREMAP_FIXED: u32 = 2;
}

/// `madvise(2)` `advice`. Only the three this kernel acts on are named; the
/// rest are accepted as no-ops by the syscall, not by this table.
pub mod madvise {
    pub const MADV_NORMAL: i32 = 0;
    pub const MADV_WILLNEED: i32 = 3;
    pub const MADV_DONTNEED: i32 = 4;
    pub const MADV_FREE: i32 = 8;
}

/// `poll(2)` / `ppoll(2)` event bits, as they appear in `pollfd.events`.
pub mod poll {
    pub const POLLIN: i16 = 0x001;
    pub const POLLPRI: i16 = 0x002;
    pub const POLLOUT: i16 = 0x004;
    pub const POLLERR: i16 = 0x008;
    pub const POLLHUP: i16 = 0x010;
    pub const POLLNVAL: i16 = 0x020;
    pub const POLLRDHUP: i16 = 0x2000;
}

/// `epoll` event bits, `epoll_ctl` operations, and `epoll_create1` flags.
pub mod epoll {
    pub const EPOLLIN: u32 = 0x001;
    pub const EPOLLPRI: u32 = 0x002;
    pub const EPOLLOUT: u32 = 0x004;
    pub const EPOLLERR: u32 = 0x008;
    pub const EPOLLHUP: u32 = 0x010;
    pub const EPOLLRDHUP: u32 = 0x2000;
    pub const EPOLLONESHOT: u32 = 1 << 30;
    pub const EPOLLET: u32 = 1 << 31;

    /// The bits this kernel actually reports back.
    ///
    /// `EPOLLET`/`EPOLLONESHOT` are *registration* bits, not readiness bits,
    /// which is why they are excluded: masking a returned `revents` with this
    /// must never let one through.
    pub const EPOLL_EVENT_MASK: u32 = EPOLLIN | EPOLLOUT | EPOLLERR | EPOLLHUP | EPOLLRDHUP;

    pub const EPOLL_CTL_ADD: i32 = 1;
    pub const EPOLL_CTL_DEL: i32 = 2;
    pub const EPOLL_CTL_MOD: i32 = 3;

    /// `EPOLL_CLOEXEC` — the same bit as `O_CLOEXEC`, which is the whole
    /// definition of it in Linux.
    pub const EPOLL_CLOEXEC: u32 = 0o2000000;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one value in this file that is a target-specific trap. If somebody
    /// "corrects" `O_DIRECTORY` to the asm-generic `0o200000`, `O_TMPFILE`
    /// stops matching what musl sends and `sys_openat` stops rejecting it — so
    /// apk-tools 3 writes into a directory fd instead of taking its fallback.
    #[test]
    fn o_directory_is_the_arm64_value_and_o_tmpfile_contains_it() {
        assert_eq!(open::O_DIRECTORY, 0o40000);
        assert_ne!(open::O_DIRECTORY, 0o200000, "that is the x86-64/riscv value");
        assert_eq!(open::O_TMPFILE & open::O_DIRECTORY, open::O_DIRECTORY);
        assert_eq!(open::O_TMPFILE, 0o20000000 | open::O_DIRECTORY);
    }

    /// `O_CLOEXEC` and `EPOLL_CLOEXEC` are the same bit — `epoll_create1` reuses
    /// it verbatim. Two spellings of one value is fine; two *different* values
    /// would mean `epoll_create1(EPOLL_CLOEXEC)` silently produced an
    /// inheritable fd.
    #[test]
    fn epoll_cloexec_is_o_cloexec() {
        assert_eq!(epoll::EPOLL_CLOEXEC, open::O_CLOEXEC);
    }

    /// `O_NONBLOCK` is `0o4000` = `0x800`. Both spellings appeared in the tree
    /// (`0o4000` nowhere, `0x800` in `fs.rs`, `pidfd.rs` and as a bare literal
    /// in `sys_openat`'s pipe branch); this pins them as one number.
    #[test]
    fn o_nonblock_is_0x800() {
        assert_eq!(open::O_NONBLOCK, 0x800);
    }

    /// The access mode is an enum in the low two bits, not a flag set: nothing
    /// else in the table may collide with it.
    #[test]
    fn open_flags_clear_the_access_mode_bits() {
        for f in [
            open::O_CREAT, open::O_EXCL, open::O_NOCTTY, open::O_TRUNC,
            open::O_APPEND, open::O_NONBLOCK, open::O_DIRECTORY,
            open::O_NOFOLLOW, open::O_CLOEXEC, open::O_PATH,
        ] {
            assert_eq!(f & open::O_ACCMODE, 0, "flag {f:#o} collides with O_ACCMODE");
        }
        assert_eq!(open::O_RDWR & open::O_ACCMODE, open::O_RDWR);
    }

    /// No two `mmap` flags may share a bit, and none may be zero — a zero flag
    /// is silently always-set under `flags & F != 0`.
    #[test]
    fn map_flags_are_distinct_nonzero_bits() {
        let all = [
            map::MAP_SHARED, map::MAP_PRIVATE, map::MAP_FIXED, map::MAP_ANONYMOUS,
            map::MAP_NORESERVE, map::MAP_POPULATE, map::MAP_STACK,
            map::MAP_FIXED_NOREPLACE,
        ];
        let mut seen = 0u32;
        for f in all {
            assert_ne!(f, 0);
            assert_eq!(f.count_ones(), 1, "{f:#x} is not a single bit");
            assert_eq!(seen & f, 0, "{f:#x} collides with an earlier MAP_ flag");
            seen |= f;
        }
    }

    /// `PROT_NONE` is genuinely zero, so `prot & PROT_NONE != 0` is always
    /// false — it must be tested with `prot == PROT_NONE`. Stated as a test
    /// because it is the one member of the `prot` table that does not behave
    /// like the others.
    #[test]
    fn prot_none_is_zero_and_the_rest_are_distinct_bits() {
        assert_eq!(prot::PROT_NONE, 0);
        assert_eq!(prot::PROT_READ | prot::PROT_WRITE | prot::PROT_EXEC, 0x7);
    }

    /// `EPOLL_EVENT_MASK` must not admit the registration-only bits.
    #[test]
    fn epoll_event_mask_excludes_registration_bits() {
        assert_eq!(epoll::EPOLL_EVENT_MASK & epoll::EPOLLET, 0);
        assert_eq!(epoll::EPOLL_EVENT_MASK & epoll::EPOLLONESHOT, 0);
        assert_eq!(epoll::EPOLL_EVENT_MASK, 0x201D);
    }

    /// `poll` and `epoll` agree on the low five bits — that is what lets
    /// `sys_ppoll` and `sys_epoll_pwait` share a readiness source.
    #[test]
    fn poll_and_epoll_share_the_low_bits() {
        assert_eq!(u32::from(poll::POLLIN.cast_unsigned()), epoll::EPOLLIN);
        assert_eq!(u32::from(poll::POLLOUT.cast_unsigned()), epoll::EPOLLOUT);
        assert_eq!(u32::from(poll::POLLERR.cast_unsigned()), epoll::EPOLLERR);
        assert_eq!(u32::from(poll::POLLHUP.cast_unsigned()), epoll::EPOLLHUP);
        assert_eq!(u32::from(poll::POLLRDHUP.cast_unsigned()), epoll::EPOLLRDHUP);
    }

    /// The `AT_*` bits are a flag set on top of `AT_FDCWD`, which is a
    /// *sentinel fd*, not a flag — it lives in a different argument.
    #[test]
    fn at_flags_are_distinct_bits_and_fdcwd_is_not_one() {
        assert_eq!(at::AT_FDCWD, -100);
        // `<sys/stat.h>`: both sentinels sit just under 1<<30, above every legal
        // nanosecond value, which is what makes them unambiguous in `tv_nsec`.
        assert_eq!(utimensat::UTIME_NOW, 0x3fff_ffff);
        assert_eq!(utimensat::UTIME_OMIT, 0x3fff_fffe);
        assert!(utimensat::UTIME_OMIT > 999_999_999);
        let all = [
            at::AT_SYMLINK_NOFOLLOW, at::AT_REMOVEDIR, at::AT_SYMLINK_FOLLOW,
            at::AT_NO_AUTOMOUNT, at::AT_EMPTY_PATH,
        ];
        let mut seen = 0u32;
        for f in all {
            assert_eq!(f.count_ones(), 1);
            assert_eq!(seen & f, 0);
            seen |= f;
        }
    }
}

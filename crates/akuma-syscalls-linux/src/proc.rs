//! Process-family wire types: `struct clone_args`, `struct rlimit`,
//! `struct sysinfo`.
//!
//! `SpawnOptions` and `ThreadCpuStat` are deliberately **not** here. They are
//! Akuma ABI, not Linux ABI, and mixing the two dilutes what this crate is for:
//! everything in it can be checked against a real Linux header, and those two
//! cannot.

/// `struct clone_args`, the `clone3(2)` argument block.
///
/// `clone3` takes a `size` alongside the pointer and Linux copies
/// `min(size, sizeof)` bytes, so a caller built against an older kernel passes a
/// shorter struct. That is why the fields are in extension order and why
/// nothing may be inserted in the middle.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct CloneArgs {
    pub flags: u64,
    pub pidfd: u64,
    pub child_tid: u64,
    pub parent_tid: u64,
    pub exit_signal: u64,
    pub stack: u64,
    pub stack_size: u64,
    pub tls: u64,
}

/// Linux `struct rlimit` (the 64-bit one — `prlimit64`/`getrlimit` on aarch64
/// share it, there is no 32-bit variant on this target).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Rlimit {
    pub rlim_cur: u64,
    pub rlim_max: u64,
}

/// Linux `struct sysinfo` on a 64-bit target — 112 bytes.
///
/// `sys_sysinfo` used to build this as a `[u8; 112]` with six
/// `core::ptr::write(ptr.add(N))` calls and a comment listing the offsets. The
/// comment was right; nothing checked that it stayed right, and a struct that
/// exists only as a comment is the exact failure this crate was extracted to
/// end. The offsets are the type's now, and asserted below.
///
/// `_align` is the 4-byte hole after `pad` that `repr(C)` would insert anyway.
/// It is spelled out because `totalhigh` landing at 88 rather than 84 is the
/// entire content of the "aarch64" in "aarch64 `struct sysinfo`".
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Sysinfo {
    pub uptime: i64,
    pub loads: [u64; 3],
    pub totalram: u64,
    pub freeram: u64,
    pub sharedram: u64,
    pub bufferram: u64,
    pub totalswap: u64,
    pub freeswap: u64,
    pub procs: u16,
    pub pad: u16,
    pub _align: u32,
    pub totalhigh: u64,
    pub freehigh: u64,
    pub mem_unit: u32,
    /// The four bytes of trailing struct padding, spelled out.
    ///
    /// Linux's `char _f[20 - 2*sizeof(long) - sizeof(int)]` is zero-length on a
    /// 64-bit target, so these four bytes are genuine `repr(C)` tail padding —
    /// and `write_user_val` copies `size_of::<T>()` bytes, padding included.
    /// `sys_sysinfo` used to build a **zeroed `[u8; 112]`**, so they went out as
    /// zeroes; as implicit padding they would go out as whatever was on the
    /// kernel stack. Naming the field is what keeps `Default` zeroing them.
    pub _f: [u8; 4],
}

/// `clone(2)` / `clone3(2)` flag bits.
pub mod clone_flags {
    pub const CSIGNAL: u64 = 0x0000_00ff;
    pub const CLONE_VM: u64 = 0x0000_0100;
    pub const CLONE_FS: u64 = 0x0000_0200;
    pub const CLONE_FILES: u64 = 0x0000_0400;
    pub const CLONE_SIGHAND: u64 = 0x0000_0800;
    pub const CLONE_PIDFD: u64 = 0x0000_1000;
    pub const CLONE_VFORK: u64 = 0x0000_4000;
    pub const CLONE_PARENT: u64 = 0x0000_8000;
    pub const CLONE_THREAD: u64 = 0x0001_0000;
    pub const CLONE_NEWNS: u64 = 0x0002_0000;
    pub const CLONE_SYSVSEM: u64 = 0x0004_0000;
    pub const CLONE_SETTLS: u64 = 0x0008_0000;
    pub const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
    pub const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
    pub const CLONE_CHILD_SETTID: u64 = 0x0100_0000;
}

/// `wait4(2)` / `waitid(2)` `options` bits.
///
/// `WUNTRACED` and `WSTOPPED` are the same bit under two names — `wait4` spells
/// it the first way, `waitid` the second. Both are here because both spellings
/// appear in the callers this kernel has to satisfy.
pub mod wait_options {
    pub const WNOHANG: i32 = 1;
    pub const WUNTRACED: i32 = 2;
    pub const WSTOPPED: i32 = 2;
    pub const WEXITED: i32 = 4;
    pub const WCONTINUED: i32 = 8;
    pub const WNOWAIT: i32 = 0x0100_0000;
}

/// `waitid(2)` `idtype` values.
pub mod wait_idtype {
    pub const P_ALL: u32 = 0;
    pub const P_PID: u32 = 1;
    pub const P_PGID: u32 = 2;
    pub const P_PIDFD: u32 = 3;
}

/// `getrlimit`/`prlimit64` resource numbers, and the "no limit" sentinel.
pub mod rlimit {
    pub const RLIMIT_CPU: u32 = 0;
    pub const RLIMIT_FSIZE: u32 = 1;
    pub const RLIMIT_DATA: u32 = 2;
    pub const RLIMIT_STACK: u32 = 3;
    pub const RLIMIT_CORE: u32 = 4;
    pub const RLIMIT_NOFILE: u32 = 7;
    pub const RLIMIT_AS: u32 = 9;
    pub const RLIM_INFINITY: u64 = u64::MAX;
}

const _: () = assert!(core::mem::size_of::<CloneArgs>() == 64);
const _: () = assert!(core::mem::offset_of!(CloneArgs, exit_signal) == 32);
const _: () = assert!(core::mem::offset_of!(CloneArgs, tls) == 56);
const _: () = assert!(core::mem::size_of::<Rlimit>() == 16);

// The offsets `sys_sysinfo`'s comment used to claim, now checked.
const _: () = assert!(core::mem::size_of::<Sysinfo>() == 112);
const _: () = assert!(core::mem::offset_of!(Sysinfo, uptime) == 0);
const _: () = assert!(core::mem::offset_of!(Sysinfo, loads) == 8);
const _: () = assert!(core::mem::offset_of!(Sysinfo, totalram) == 32);
const _: () = assert!(core::mem::offset_of!(Sysinfo, freeram) == 40);
const _: () = assert!(core::mem::offset_of!(Sysinfo, sharedram) == 48);
const _: () = assert!(core::mem::offset_of!(Sysinfo, bufferram) == 56);
const _: () = assert!(core::mem::offset_of!(Sysinfo, totalswap) == 64);
const _: () = assert!(core::mem::offset_of!(Sysinfo, freeswap) == 72);
const _: () = assert!(core::mem::offset_of!(Sysinfo, procs) == 80);
const _: () = assert!(core::mem::offset_of!(Sysinfo, totalhigh) == 88);
const _: () = assert!(core::mem::offset_of!(Sysinfo, freehigh) == 96);
const _: () = assert!(core::mem::offset_of!(Sysinfo, mem_unit) == 104);

#[cfg(test)]
mod tests {
    use super::*;

    /// `clone3` truncates to the caller's `size`, so a short copy must still
    /// land `flags` and `pidfd` correctly — the property that makes the field
    /// order unchangeable.
    ///
    /// Stated as the layout invariant that *makes* a prefix copy work, rather
    /// than by performing one: every field a five-field caller knows about must
    /// end at or before `stack` begins, and every later field must start at or
    /// after it. Simulating the copy tested the same fact through a `memcpy`
    /// that could only ever agree with the offsets it was derived from.
    #[test]
    fn clone_args_prefix_survives_a_short_copy() {
        let a = CloneArgs::default();
        let cut = core::mem::offset_of!(CloneArgs, stack);
        for (name, off, size) in [
            ("flags", core::mem::offset_of!(CloneArgs, flags), core::mem::size_of_val(&a.flags)),
            ("pidfd", core::mem::offset_of!(CloneArgs, pidfd), core::mem::size_of_val(&a.pidfd)),
            (
                "child_tid",
                core::mem::offset_of!(CloneArgs, child_tid),
                core::mem::size_of_val(&a.child_tid),
            ),
            (
                "parent_tid",
                core::mem::offset_of!(CloneArgs, parent_tid),
                core::mem::size_of_val(&a.parent_tid),
            ),
            (
                "exit_signal",
                core::mem::offset_of!(CloneArgs, exit_signal),
                core::mem::size_of_val(&a.exit_signal),
            ),
        ] {
            assert!(off + size <= cut, "{name} must fit entirely in the short prefix");
        }
        for (name, off) in [
            ("stack", core::mem::offset_of!(CloneArgs, stack)),
            ("stack_size", core::mem::offset_of!(CloneArgs, stack_size)),
            ("tls", core::mem::offset_of!(CloneArgs, tls)),
        ] {
            assert!(off >= cut, "{name} must lie past the short prefix");
        }
        assert_eq!(cut, 40, "five 8-byte fields");
        assert_eq!(core::mem::size_of::<CloneArgs>(), 64);
    }

    /// The whole point of `_align`: `totalhigh` is at 88, not 84. A packed or
    /// 32-bit-`long` layout would put it at 84 and every field after it would
    /// be four bytes out — `free(1)` would print nonsense and nothing would
    /// crash.
    #[test]
    fn sysinfo_procs_and_totalhigh_straddle_the_alignment_hole() {
        let si = Sysinfo::default();
        assert_eq!(core::mem::offset_of!(Sysinfo, procs), 80);
        assert_eq!(core::mem::size_of_val(&si.procs), 2);
        assert_eq!(core::mem::offset_of!(Sysinfo, _align), 84, "the hole, spelled out");
        assert_eq!(core::mem::size_of_val(&si._align), 4);
        assert_eq!(core::mem::offset_of!(Sysinfo, totalhigh), 88, "88, not 84");
    }

    /// Every byte of a defaulted `Sysinfo` is zero, tail padding included.
    ///
    /// `write_user_val` copies `size_of::<T>()` bytes straight out of the value,
    /// so an unnamed padding byte is a kernel-stack byte handed to userspace.
    /// `sys_sysinfo` used to build a zeroed `[u8; 112]` and got this for free;
    /// the explicit `_f` field is what preserves it.
    #[test]
    fn defaulted_sysinfo_has_no_uninitialised_bytes() {
        let si = Sysinfo::default();
        // Every byte is accounted for by a *named* field, so there is no
        // implicit padding for the compiler to leave uninitialised — which is
        // the property that matters, and a stronger statement than "the bytes
        // happened to be zero this run". Reading padding to check it would
        // itself be undefined behaviour.
        let named: usize = [
            core::mem::size_of_val(&si.uptime),
            core::mem::size_of_val(&si.loads),
            core::mem::size_of_val(&si.totalram),
            core::mem::size_of_val(&si.freeram),
            core::mem::size_of_val(&si.sharedram),
            core::mem::size_of_val(&si.bufferram),
            core::mem::size_of_val(&si.totalswap),
            core::mem::size_of_val(&si.freeswap),
            core::mem::size_of_val(&si.procs),
            core::mem::size_of_val(&si.pad),
            core::mem::size_of_val(&si._align),
            core::mem::size_of_val(&si.totalhigh),
            core::mem::size_of_val(&si.freehigh),
            core::mem::size_of_val(&si.mem_unit),
            core::mem::size_of_val(&si._f),
        ]
        .iter()
        .sum();
        assert_eq!(
            named,
            core::mem::size_of::<Sysinfo>(),
            "an unnamed padding byte is a kernel-stack byte handed to userspace"
        );
        // ...and every one of those named fields defaults to zero, including the
        // three that exist only to occupy space (`pad`, `_align`, `_f`).
        assert_eq!(si.uptime, 0);
        assert_eq!(si.loads, [0; 3]);
        assert_eq!(
            (si.totalram, si.freeram, si.sharedram, si.bufferram),
            (0, 0, 0, 0)
        );
        assert_eq!((si.totalswap, si.freeswap), (0, 0));
        assert_eq!((si.procs, si.pad, si._align), (0, 0, 0));
        assert_eq!((si.totalhigh, si.freehigh), (0, 0));
        assert_eq!(si.mem_unit, 0);
        assert_eq!(si._f, [0; 4]);
    }

    /// The four `clone` flags `sys_clone_pidfd` branches on, against the values
    /// in `<linux/sched.h>`. They were function-local `const`s in four different
    /// bodies before this crate.
    #[test]
    fn clone_flag_values_match_linux() {
        use clone_flags::*;
        assert_eq!(CLONE_VM, 0x100);
        assert_eq!(CLONE_PIDFD, 0x1000);
        assert_eq!(CLONE_VFORK, 0x4000);
        assert_eq!(CLONE_THREAD, 0x10000);
        // The low byte is the exit signal, not a flag — `clone3` ORs
        // `exit_signal` into `flags`, so any flag overlapping CSIGNAL would be
        // set by a plain `SIGCHLD` fork.
        for f in [CLONE_VM, CLONE_PIDFD, CLONE_VFORK, CLONE_THREAD, CLONE_SETTLS] {
            assert_eq!(f & CSIGNAL, 0);
        }
    }

    #[test]
    fn wnowait_is_bit_24_not_a_low_bit() {
        assert_eq!(wait_options::WNOWAIT, 1 << 24);
        assert_eq!(wait_options::WNOHANG & wait_options::WNOWAIT, 0);
    }
}

/// `sched_getaffinity(2)` / `sched_setaffinity(2)` write the CPU mask as an array
/// of `unsigned long`, so one word covers 64 CPUs.
///
/// Akuma's SMP scope is at most 64 cores, so exactly one word is ever written and
/// the syscall returns `min(cpusetsize, CPU_SET_WORD_BYTES)`. That return value is
/// load-bearing rather than cosmetic: musl's wrapper zeroes the remainder of the
/// caller's buffer from it (`if (r < size) memset(mask+r, 0, size-r)`), so
/// returning 0 makes musl wipe the whole mask and `nproc` reports no CPUs at all.
pub const CPU_SET_WORD_BYTES: usize = core::mem::size_of::<u64>();

/// The most CPUs one [`CPU_SET_WORD_BYTES`] word can describe.
pub const CPU_SET_BITS_PER_WORD: usize = CPU_SET_WORD_BYTES * 8;

const _: () = assert!(CPU_SET_BITS_PER_WORD == 64);

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
    #[test]
    fn clone_args_prefix_survives_a_short_copy() {
        let full = CloneArgs {
            flags: 0x1_0000,
            pidfd: 0xAAAA,
            child_tid: 0xBBBB,
            parent_tid: 0xCCCC,
            exit_signal: 17,
            stack: 0xDDDD,
            stack_size: 0x1000,
            tls: 0xEEEE,
        };
        let raw: [u8; 64] = unsafe { core::mem::transmute(full) };

        // A caller built against a kernel that only knew the first five fields.
        let mut short = CloneArgs::default();
        let n = core::mem::offset_of!(CloneArgs, stack);
        unsafe {
            core::ptr::copy_nonoverlapping(
                raw.as_ptr(),
                core::ptr::from_mut(&mut short).cast::<u8>(),
                n,
            );
        }
        assert_eq!(short.flags, 0x1_0000);
        assert_eq!(short.exit_signal, 17);
        assert_eq!(short.stack, 0, "past the copied prefix");
        assert_eq!(short.tls, 0);
    }

    /// The whole point of `_align`: `totalhigh` is at 88, not 84. A packed or
    /// 32-bit-`long` layout would put it at 84 and every field after it would
    /// be four bytes out — `free(1)` would print nonsense and nothing would
    /// crash.
    #[test]
    fn sysinfo_procs_and_totalhigh_straddle_the_alignment_hole() {
        let mut si = Sysinfo::default();
        si.procs = 0x1234;
        si.totalhigh = 0x0102_0304_0506_0708;
        let raw: [u8; 112] = unsafe { core::mem::transmute(si) };
        assert_eq!(u16::from_le_bytes(raw[80..82].try_into().unwrap()), 0x1234);
        assert_eq!(u32::from_le_bytes(raw[84..88].try_into().unwrap()), 0, "_align");
        assert_eq!(
            u64::from_le_bytes(raw[88..96].try_into().unwrap()),
            0x0102_0304_0506_0708
        );
    }

    /// Every byte of a defaulted `Sysinfo` is zero, tail padding included.
    ///
    /// `write_user_val` copies `size_of::<T>()` bytes straight out of the value,
    /// so an unnamed padding byte is a kernel-stack byte handed to userspace.
    /// `sys_sysinfo` used to build a zeroed `[u8; 112]` and got this for free;
    /// the explicit `_f` field is what preserves it.
    #[test]
    fn defaulted_sysinfo_has_no_uninitialised_bytes() {
        let raw: [u8; 112] = unsafe { core::mem::transmute(Sysinfo::default()) };
        assert!(raw.iter().all(|b| *b == 0), "padding byte is not zeroed");
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

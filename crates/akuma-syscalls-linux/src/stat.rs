//! `struct stat`, `struct statx`, `struct statfs`, and `makedev`.
//!
//! These are the buffers `fstat`/`newfstatat`/`statx`/`statfs` write into
//! userspace, so a wrong offset here does not crash — it makes `ls` print the
//! wrong size, or `apk` decide a file is a directory. That is the failure mode
//! this crate exists for: invisible at the call site, invisible in a boot log,
//! and caught in a millisecond by an `offset_of!` assertion.

/// `struct stat` as aarch64 Linux defines it (`asm-generic/stat.h`), 128 bytes.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Stat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_mode: u32,
    pub st_nlink: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub st_rdev: u64,
    pub __pad1: u64,
    pub st_size: i64,
    pub st_blksize: i32,
    pub __pad2: i32,
    pub st_blocks: i64,
    pub st_atime: i64,
    pub st_atime_nsec: i64,
    pub st_mtime: i64,
    pub st_mtime_nsec: i64,
    pub st_ctime: i64,
    pub st_ctime_nsec: i64,
    pub __unused: [i32; 2],
}

/// `struct statx_timestamp` — 16 bytes, and the `__reserved` word is why it is
/// not just a pair.
///
/// It was spelled twice in `src/syscall/fs.rs` (once as a lowercase
/// `statx_timestamp`), which is the third of the five-way duplication the
/// proposal for this crate opened with.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct StatxTimestamp {
    pub tv_sec: i64,
    pub tv_nsec: u32,
    pub __reserved: i32,
}

/// `struct statx` (256 bytes), the `statx(2)` buffer.
///
/// `sys_statx` used to fill this with 20 `core::ptr::write(p.add(N).cast::<T>())`
/// calls into a stack buffer, each offset a literal beside a comment naming the
/// field — `UNSAFE_AUDIT.md` §4 P1's third block. The offsets are the struct's
/// now, and the assertions below pin the ones the old comments claimed.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Statx {
    pub stx_mask: u32,
    pub stx_blksize: u32,
    pub stx_attributes: u64,
    pub stx_nlink: u32,
    pub stx_uid: u32,
    pub stx_gid: u32,
    pub stx_mode: u16,
    pub __spare0: u16,
    pub stx_ino: u64,
    pub stx_size: u64,
    pub stx_blocks: u64,
    pub stx_attributes_mask: u64,
    pub stx_atime: StatxTimestamp,
    pub stx_btime: StatxTimestamp,
    pub stx_ctime: StatxTimestamp,
    pub stx_mtime: StatxTimestamp,
    pub stx_rdev_major: u32,
    pub stx_rdev_minor: u32,
    pub stx_dev_major: u32,
    pub stx_dev_minor: u32,
    pub stx_mnt_id: u64,
    pub stx_dio_mem_align: u32,
    pub stx_dio_offset_align: u32,
    pub __spare3: [u64; 12],
}

/// `struct statfs` as `fstatfs`/`statfs` fill it on aarch64 (120 bytes).
///
/// Was function-local inside `sys_statfs_common`, so nothing could assert its
/// size — and its size is the whole contract: musl copies `f_spare` too.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Statfs {
    pub f_type: i64,
    pub f_bsize: i64,
    pub f_blocks: i64,
    pub f_bfree: i64,
    pub f_bavail: i64,
    pub f_files: i64,
    pub f_ffree: i64,
    pub f_fsid: [i32; 2],
    pub f_namelen: i64,
    pub f_frsize: i64,
    pub f_flags: i64,
    pub f_spare: [i64; 4],
}

/// Linux's `makedev` for the values that cross this ABI: an 8-bit minor packed
/// under the major.
#[must_use]
pub const fn makedev(major: u64, minor: u64) -> u64 {
    (major << 8) | minor
}

// The offsets `sys_statx` used to spell as literals. A layout change that moves
// any of them is a build failure rather than a userspace `stat` reading the
// wrong field.
const _: () = assert!(core::mem::size_of::<Statx>() == 256);
const _: () = assert!(core::mem::offset_of!(Statx, stx_nlink) == 16);
const _: () = assert!(core::mem::offset_of!(Statx, stx_mode) == 28);
const _: () = assert!(core::mem::offset_of!(Statx, stx_ino) == 32);
const _: () = assert!(core::mem::offset_of!(Statx, stx_size) == 40);
const _: () = assert!(core::mem::offset_of!(Statx, stx_blocks) == 48);
const _: () = assert!(core::mem::offset_of!(Statx, stx_atime) == 64);
const _: () = assert!(core::mem::offset_of!(Statx, stx_btime) == 80);
const _: () = assert!(core::mem::offset_of!(Statx, stx_ctime) == 96);
const _: () = assert!(core::mem::offset_of!(Statx, stx_mtime) == 112);
const _: () = assert!(core::mem::offset_of!(Statx, stx_rdev_major) == 128);
const _: () = assert!(core::mem::offset_of!(Statx, stx_dev_major) == 136);
const _: () = assert!(core::mem::offset_of!(Statx, stx_mnt_id) == 144);

// `struct stat` had no assertions at all before this crate — it is the buffer
// every `ls`, every `apk`, and every `cargo` stat call reads, and nothing
// pinned a single offset of it.
const _: () = assert!(core::mem::size_of::<Stat>() == 128);
const _: () = assert!(core::mem::offset_of!(Stat, st_mode) == 16);
const _: () = assert!(core::mem::offset_of!(Stat, st_rdev) == 32);
const _: () = assert!(core::mem::offset_of!(Stat, st_size) == 48);
const _: () = assert!(core::mem::offset_of!(Stat, st_blksize) == 56);
const _: () = assert!(core::mem::offset_of!(Stat, st_blocks) == 64);
const _: () = assert!(core::mem::offset_of!(Stat, st_atime) == 72);
const _: () = assert!(core::mem::offset_of!(Stat, st_mtime) == 88);
const _: () = assert!(core::mem::offset_of!(Stat, st_ctime) == 104);

const _: () = assert!(core::mem::size_of::<StatxTimestamp>() == 16);
const _: () = assert!(core::mem::size_of::<Statfs>() == 120);
const _: () = assert!(core::mem::offset_of!(Statfs, f_fsid) == 56);
const _: () = assert!(core::mem::offset_of!(Statfs, f_namelen) == 64);
const _: () = assert!(core::mem::offset_of!(Statfs, f_spare) == 88);

#[cfg(test)]
mod tests {
    use super::*;

    /// The `st_size` a 64-bit `ls` reads must land at byte 48 and be 8 bytes
    /// wide. Offset *and* width, because either alone is insufficient: a
    /// `st_size: i32` could keep every later offset if padding absorbed it, and
    /// a right-width field at the wrong offset shifts everything after it.
    #[test]
    fn stat_size_lands_at_offset_48() {
        let st = Stat::default();
        assert_eq!(core::mem::offset_of!(Stat, st_size), 48);
        assert_eq!(core::mem::size_of_val(&st.st_size), 8);
    }

    /// `st_blksize` is `i32` followed by `__pad2`: writing it must not disturb
    /// `st_blocks` at 64. This is the shape of the bug the assertions catch —
    /// one field's width wrong, everything after it shifted by four bytes.
    #[test]
    fn stat_blksize_is_32_bit_and_padded() {
        let st = Stat::default();
        assert_eq!(core::mem::offset_of!(Stat, st_blksize), 56);
        assert_eq!(core::mem::size_of_val(&st.st_blksize), 4, "i32, not i64");
        assert_eq!(core::mem::offset_of!(Stat, __pad2), 60, "the explicit filler");
        assert_eq!(core::mem::size_of_val(&st.__pad2), 4);
        assert_eq!(core::mem::offset_of!(Stat, st_blocks), 64, "undisturbed by the pair above");
    }

    /// The four `statx_timestamp`s are a stride of 16 starting at 64 — the
    /// property the four separate `offset_of!` assertions state one at a time.
    #[test]
    fn statx_timestamps_are_a_stride_of_16() {
        let offs = [
            core::mem::offset_of!(Statx, stx_atime),
            core::mem::offset_of!(Statx, stx_btime),
            core::mem::offset_of!(Statx, stx_ctime),
            core::mem::offset_of!(Statx, stx_mtime),
        ];
        for (i, off) in offs.iter().enumerate() {
            assert_eq!(*off, 64 + i * core::mem::size_of::<StatxTimestamp>());
        }
    }

    #[test]
    fn makedev_packs_minor_in_the_low_byte() {
        assert_eq!(makedev(1, 3), 0x103); // /dev/null
        assert_eq!(makedev(1, 5), 0x105); // /dev/zero
        assert_eq!(makedev(0, 0), 0);
    }
}

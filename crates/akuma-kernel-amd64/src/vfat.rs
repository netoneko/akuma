//! `mount -t vfat`: the EFI System Partition (and any other FAT volume) behind
//! [`akuma_vfs_glue`]'s mount table. Feature `vfat`, amd64 only.
//!
//! `mount(2)` itself is glue's, shared with the AArch64 kernel, and its `fstype`
//! switch knows `proc`, `tmpfs` and `ext2`. Rather than add an arm there (which
//! would put the FAT crate in the AArch64 image and a block-device lookup glue
//! cannot make for NVMe), the amd64 dispatcher offers every `mount` to
//! [`try_mount`] first: it answers for the FAT spellings and returns `None` for
//! anything else, which then goes to glue unchanged.
//!
//! # Sources
//!
//! | `source`            | device |
//! |---------------------|--------|
//! | `/dev/vdb` ...      | a virtio-blk disk, whole (a FAT volume with no partition table, as a VMM test image is) |
//! | `/dev/nvme0n1pN`    | GPT partition `N` of the NVMe disk, through the driver's **auxiliary window** (`nvme::open_aux_partition`), which refuses the root's partition and anything overlapping it |
//!
//! Permission is glue's: the host root (box 0, uid 0), the same token
//! `sys_mount` demands. Only one NVMe partition can be mounted this way at a
//! time; the window is closed when the filesystem is dropped (unmount), which
//! flushes the controller's cache.
//!
//! The clock is `clock::now_us`, which is 0 until the SNTP client syncs; a write
//! before then is stamped 1980 (the FAT epoch), which is honest rather than
//! plausible.

use crate::fd::errno;
use akuma_vfat::{BlockDevice, Vfat};
use alloc::string::String;
use alloc::sync::Arc;

/// `ENOMEM`, `EBUSY`, `ENOTDIR`, `EROFS`, `EIO`: not in `fd::errno`.
const ENOMEM: u64 = (-12i64) as u64;
const EBUSY: u64 = (-16i64) as u64;
const ENOTDIR: u64 = (-20i64) as u64;
const EROFS: u64 = (-30i64) as u64;
const EIO: u64 = (-5i64) as u64;

/// What a FAT volume is sitting on.
pub enum FatDisk {
    /// Virtio-blk device `idx` (`vda` is 0), whole.
    Virtio(usize),
    /// The NVMe driver's auxiliary partition window.
    Nvme,
}

impl BlockDevice for FatDisk {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        match self {
            Self::Virtio(i) => akuma_virtio::block::read_bytes_at(*i, offset, buf).map_err(|_| ()),
            Self::Nvme => crate::drivers::nvme_aux_read(offset, buf).map_err(|_| ()),
        }
    }

    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
        match self {
            Self::Virtio(i) => akuma_virtio::block::write_bytes_at(*i, offset, data).map_err(|_| ()),
            Self::Nvme => crate::drivers::nvme_aux_write(offset, data).map_err(|_| ()),
        }
    }

    fn flush(&self) -> Result<(), ()> {
        match self {
            // The cache in front of a VMM disk is the host's; see `fs::VirtioBlk`.
            Self::Virtio(_) => Ok(()),
            Self::Nvme => crate::drivers::nvme_flush().map_err(|_| ()),
        }
    }
}

impl Drop for FatDisk {
    fn drop(&mut self) {
        if matches!(self, Self::Nvme) {
            crate::drivers::nvme_close_aux_partition();
        }
    }
}

fn clock_secs() -> u64 {
    crate::clock::now_us() / 1_000_000
}

fn is_fat_type(t: &str) -> bool {
    matches!(t, "vfat" | "msdos" | "fat" | "fat32" | "fat16" | "fat12")
}

/// `nvme0n1pN` -> `N`.
fn nvme_partition(source: &str) -> Option<u32> {
    let n = source.strip_prefix("/dev/").unwrap_or(source).strip_prefix("nvme0n1p")?;
    n.parse().ok().filter(|&n| n >= 1)
}

/// Serve `mount(source, target, fstype, flags, data)` if `fstype` is a FAT
/// spelling. `None`: not ours, hand the call to glue.
#[must_use]
pub fn try_mount(source_ptr: u64, target_ptr: u64, fstype_ptr: u64, flags: u64) -> Option<u64> {
    let fstype = crate::uaccess::read_cstr(fstype_ptr, 64)?;
    let fstype = String::from_utf8(fstype).ok()?;
    // A remount names no new device: glue flips the flags of whatever is mounted there.
    if !is_fat_type(&fstype) || flags & akuma_vfs::MS_REMOUNT != 0 {
        return None;
    }
    Some(mount_fat(source_ptr, target_ptr, flags))
}

fn mount_fat(source_ptr: u64, target_ptr: u64, flags: u64) -> u64 {
    let Some(root) = akuma_exec::process::host_root() else {
        return errno::EPERM;
    };
    let (Some(target), Some(source)) = (
        crate::uaccess::read_cstr(target_ptr, 256).and_then(|b| String::from_utf8(b).ok()),
        crate::uaccess::read_cstr(source_ptr, 64).and_then(|b| String::from_utf8(b).ok()),
    ) else {
        return errno::EFAULT;
    };
    let target = akuma_vfs_glue::canonicalize_path(&target);
    match akuma_vfs_glue::metadata(&target) {
        Ok(m) if m.is_dir => {}
        Ok(_) => return ENOTDIR,
        Err(akuma_vfs::FsError::NotADirectory) => return ENOTDIR,
        Err(_) => return errno::ENOENT,
    }

    let disk = if let Some(idx) = akuma_virtio::block::device_index_by_name(&source) {
        FatDisk::Virtio(idx)
    } else if let Some(part) = nvme_partition(&source) {
        match crate::drivers::nvme_open_aux_partition(part) {
            Ok(_) => FatDisk::Nvme,
            Err(e) => {
                akuma_primitives::safe_print!(160, "[vfat] {}: {}\n", source, e);
                return if e.contains("already open") || e.contains("root") || e.contains("overlaps") {
                    EBUSY
                } else {
                    errno::ENODEV
                };
            }
        }
    } else {
        return errno::ENODEV;
    };

    // `FatDisk::Nvme` closes its window on drop, so every early return below is clean.
    let fs = match Vfat::mount(disk, clock_secs) {
        Ok(fs) => fs,
        Err(akuma_vfs::FsError::IoError) => return EIO,
        Err(_) => return errno::EINVAL, // no FAT boot sector on the device
    };
    let fs: Arc<dyn akuma_vfs::Filesystem> = Arc::new(fs);
    match akuma_vfs_glue::mount_with(root, &target, Some(&source), flags, fs) {
        Ok(()) => 0,
        Err(akuma_vfs::FsError::NoSpace) => ENOMEM,
        Err(akuma_vfs::FsError::AlreadyExists) => EBUSY,
        Err(akuma_vfs::FsError::NotFound) => errno::ENOENT,
        Err(akuma_vfs::FsError::NotADirectory) => ENOTDIR,
        Err(akuma_vfs::FsError::ReadOnly) => EROFS,
        Err(_) => errno::EINVAL,
    }
}

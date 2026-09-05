//! Stage N: a filesystem.
//!
//! `akuma-ext2` mounted on the virtio-blk device from Stage M, so the kernel can
//! open a file by path. Like the block driver before it, the ext2 code is used
//! **unmodified** — it already built for `x86_64-unknown-none`, it already
//! forbids `unsafe`, and its whole interface to a disk is two methods:
//!
//! ```ignore
//! pub trait BlockDevice: Send + Sync {
//!     fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()>;
//!     fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()>;
//! }
//! ```
//!
//! which is exactly what `akuma_virtio::block` exposes. The shim below is the
//! entire adaptation layer, and its brevity is the finding rather than an
//! accident: the seam was drawn in the right place years before this target
//! existed.
//!
//! # What this does not do
//!
//! There is no mount table and no path namespace — one filesystem, reached
//! through [`with_root`]. `akuma-vfs`'s `MountTable` is what generalises that,
//! and it is not needed to open one file on one disk. It arrives when there is a
//! second filesystem to mount, which is the point at which a mount table stops
//! being ceremony.
//!
//! Writes are exercised now (2026-09-04, [`write_file`]) — `fd::sys_write` on a
//! file opened `O_CREAT`/`O_WRONLY` buffers into the descriptor's own `Vec<u8>`
//! and this is called once, at `close(2)`, to persist it. `fd::smoke_test`
//! writes and reads one back as part of the boot self-tests: the old worry
//! about a mutating self-test making the image stateful across boots does not
//! apply here, because `run.sh` already rebuilds the image on every run (see
//! this file's own module header, further up) — nothing depends on this image
//! surviving to the next boot unchanged.

use akuma_ext2::{BlockDevice, Ext2Filesystem};
use akuma_selftest::Suite;
use akuma_vfs::{DirEntry, Filesystem, FsError, Metadata};
use alloc::string::String;
use alloc::vec::Vec;
use spinning_top::Spinlock;

use crate::serial;

/// The virtio-blk device, as something `akuma-ext2` can read.
///
/// Device 0 — `vda`, the first disk the machine announced. A second disk would
/// need a second instance of this, which is where a mount table starts earning
/// its keep.
pub struct VirtioBlk;

impl BlockDevice for VirtioBlk {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        akuma_virtio::block::read_bytes(offset, buf).map_err(|_| ())
    }

    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
        akuma_virtio::block::write_bytes(offset, data).map_err(|_| ())
    }
}

/// Whatever the root filesystem is sitting on.
///
/// Two things can be, and they arrive by completely different routes: a
/// virtio-blk disk from a VMM, or a span of RAM that GRUB filled with an ext2
/// image before handing over. An enum rather than a `dyn` object because
/// `Ext2Filesystem` is generic over its device and there are exactly two, both
/// known at compile time -- a trait object would cost a vtable dispatch per
/// block read to express a choice made once at boot.
pub enum RootDevice {
    /// A virtio-blk disk: `vda`, from a VMM.
    Virtio(VirtioBlk),
    /// An ext2 image already in memory, placed there by the boot loader.
    Ram(crate::ramdisk::RamDisk),
}

impl BlockDevice for RootDevice {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        match self {
            RootDevice::Virtio(d) => d.read_bytes(offset, buf),
            RootDevice::Ram(d) => d.read_bytes(offset, buf),
        }
    }

    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
        match self {
            RootDevice::Virtio(d) => d.write_bytes(offset, data),
            RootDevice::Ram(d) => d.write_bytes(offset, data),
        }
    }
}

/// The mounted root filesystem.
///
/// A `Spinlock<Option<..>>` rather than a `OnceCell`: mounting can fail (no
/// disk, not ext2, a corrupt superblock) and the kernel must boot anyway, so
/// "not mounted" has to be a representable state rather than a panic.
static ROOT: Spinlock<Option<Ext2Filesystem<RootDevice>>> = Spinlock::new(None);

/// Wall-clock source for inode timestamps.
///
/// Zero, honestly. This target has a LAPIC timer but no RTC and no SNTP client,
/// so it does not know what time it is; `Ext2Filesystem::new` documents `|| 0`
/// as the answer for exactly this case. Every file this kernel writes would be
/// stamped 1970 — which is why nothing writes yet.
fn no_clock() -> u64 {
    0
}

/// Mount the first block device as the root filesystem.
///
/// Returns false when there is no disk or it does not hold an ext2 image.
/// Neither is fatal: the kernel booted without a filesystem for every stage
/// before this one, and `DISK=none` still has to work.
pub fn mount_root() -> bool {
    if !akuma_virtio::block::is_initialized() {
        return false;
    }
    mount_root_on(RootDevice::Virtio(VirtioBlk), "vda")
}

/// Mount `device` as the root filesystem, naming it in the diagnostics.
///
/// The bare-metal path comes here with a [`RootDevice::Ram`]: an ext2 image the
/// boot loader left in memory, since a machine with no storage driver still
/// needs somewhere for `/bin/sh` to live.
pub fn mount_root_on(device: RootDevice, name: &str) -> bool {
    // Not an error worth halting for: a raw disk with no filesystem is a
    // legitimate thing to be handed, and the message says which happened.
    let Ok(fs) = Ext2Filesystem::new(device, no_clock) else {
        serial::puts("  fs:   ");
        serial::puts(name);
        serial::puts(" holds no readable ext2 image\n");
        return false;
    };
    serial::puts("  fs:   ext2 mounted on ");
    serial::puts(name);
    serial::puts("\n");
    *ROOT.lock() = Some(fs);
    true
}

/// Run `f` against the root filesystem, if one is mounted.
///
/// A closure rather than a returned reference: the filesystem lives behind a
/// lock, and handing out a borrow would mean handing out the guard's lifetime
/// to callers that have no reason to think about it.
pub fn with_root<R>(f: impl FnOnce(&Ext2Filesystem<RootDevice>) -> R) -> Option<R> {
    ROOT.lock().as_ref().map(f)
}

/// Read a whole file from the root filesystem.
#[must_use]
pub fn read_file(path: &str) -> Option<Vec<u8>> {
    with_root(|fs| fs.read_file(path).ok())?
}

/// Write a whole file to the root filesystem, creating it if it does not
/// exist. The first real write path on this target — `akuma-ext2`'s
/// `write_file` (create-or-truncate-and-replace) was always here, unmodified
/// and untouched since Stage N; nothing on amd64 called it before `fd`'s
/// close-time flush (see that module's header for why the write is buffered
/// in memory and only lands here once).
///
/// `false` covers "no filesystem mounted" and every `akuma-ext2` failure
/// (most commonly: the parent directory does not exist — `write_file` does
/// not create one, matching `open(2)`'s own contract).
pub fn write_file(path: &str, data: &[u8]) -> Result<(), FsError> {
    with_root(|fs| fs.write_file(path, data)).unwrap_or(Err(FsError::NoFilesystem))
}

/// Create a directory. `mkdirat(2)`'s body — the parent must already exist,
/// matching `akuma-ext2`'s (and `mkdir(2)`'s) own contract. First consumer:
/// `apk`'s cache-directory setup.
pub fn create_dir(path: &str) -> Result<(), FsError> {
    with_root(|fs| fs.create_dir(path)).unwrap_or(Err(FsError::NoFilesystem))
}

/// Remove a file (or, with `rmdir`, an empty directory). `unlinkat(2)`'s body.
pub fn remove(path: &str, rmdir: bool) -> Result<(), FsError> {
    with_root(|fs| {
        if rmdir {
            fs.remove_dir(path)
        } else {
            fs.remove_file(path)
        }
    })
    .unwrap_or(Err(FsError::NoFilesystem))
}

/// Rename (move) a path. `renameat(2)`'s body — the target is replaced if it
/// exists, which is the atomic-tmpfile-swap shape `apk` names this syscall
/// for. First consumer: `apk`'s `.tmp.<pid>` + rename cache write.
pub fn rename(old_path: &str, new_path: &str) -> Result<(), FsError> {
    with_root(|fs| fs.rename(old_path, new_path)).unwrap_or(Err(FsError::NoFilesystem))
}

/// Create a symlink. `symlinkat(2)`'s body. First consumer: `apk add` —
/// package contents carry symlinks (`.so.1` versioned-library names), and
/// every one of them failed with ENOSYS until this existed.
pub fn create_symlink(link_path: &str, target: &str) -> Result<(), FsError> {
    with_root(|fs| fs.create_symlink(link_path, target)).unwrap_or(Err(FsError::NoFilesystem))
}

/// Read a symlink's target. `readlink(2)`'s body.
pub fn read_symlink(path: &str) -> Result<String, FsError> {
    with_root(|fs| fs.read_symlink(path)).unwrap_or(Err(FsError::NoFilesystem))
}

/// Set file timestamps. `utimensat(2)`'s body; `None` leaves a stamp alone
/// (`UTIME_OMIT`), matching the VFS trait's contract. First consumer: `apk`'s
/// "preserve owner mtime" pass over extracted files.
pub fn set_times(path: &str, atime_secs: Option<u64>, mtime_secs: Option<u64>) -> Result<(), FsError> {
    with_root(|fs| fs.set_times(path, atime_secs, mtime_secs)).unwrap_or(Err(FsError::NoFilesystem))
}

/// Inode metadata for a path — the backing for the path-based `stat` syscalls.
///
/// `akuma-ext2`'s `type_perms` maps straight onto a Linux `st_mode`, so the
/// caller gets the real file type and permission bits, not a fixed guess. The
/// path walk does not follow symlinks (see [`sys_newfstatat`]'s note).
///
/// [`sys_newfstatat`]: crate::fd::sys_newfstatat
#[must_use]
pub fn metadata(path: &str) -> Option<Metadata> {
    with_root(|fs| fs.metadata(path).ok())?
}

/// List a directory's entries — the backing for `getdents64` (`ls`, `find`).
///
/// `akuma-ext2`'s `read_dir` already drops the synthetic `.`/`..` records (it
/// filters them out of the raw directory block before returning), so this
/// target's `getdents64` never has to invent them — the same shape the
/// AArch64 kernel's `list_dir` hands its own `sys_getdents64`.
///
/// `None` covers both "no filesystem mounted" and "not a directory" — the
/// caller (`fd::sys_getdents64`) only has one error to report either way.
#[must_use]
pub fn read_dir(path: &str) -> Option<Vec<DirEntry>> {
    with_root(|fs| fs.read_dir(path).ok())?
}

/// Mount, then prove the filesystem can be read.
pub fn smoke_test(t: &mut Suite, mounted: bool) {
    if !t.check("fs: ext2 mounted", mounted) {
        return;
    }

    // Directory listing first. It is the cheapest operation that proves the
    // inode table, the block groups and the directory-entry walk all agree — a
    // driver that could read a file by inode number but not resolve a name
    // would still fail here.
    let names = with_root(|fs| {
        fs.read_dir("/").map(|entries| {
            let mut has_bin = false;
            let mut has_probe = false;
            for e in &entries {
                if e.name == "bin" {
                    has_bin = true;
                }
                if e.name == "probe.txt" {
                    has_probe = true;
                }
            }
            (entries.len(), has_bin, has_probe)
        })
    });
    let Some(Ok((n, has_bin, has_probe))) = names else {
        t.check("fs: read_dir /", false);
        return;
    };
    t.note("fs: entries in /", n as u64);
    t.check("fs: / contains bin/", has_bin);
    t.check("fs: / contains probe.txt", has_probe);

    // A file with known contents, checked byte by byte. `mkdisk.sh` writes a
    // header line then 200 numbered lines; a short read or a wrong block would
    // survive a length check and fail this.
    let Some(text) = read_file("/probe.txt") else {
        t.check("fs: read /probe.txt", false);
        return;
    };
    t.check("fs: read /probe.txt", true);
    t.check(
        "fs: probe.txt starts with its signature",
        text.starts_with(b"AKUMA/amd64 ext2 probe\n"),
    );
    t.check_eq("fs: probe.txt length", text.len() as u64, 6623);
    // The last line, so the tail of a multi-block file is checked as well as the
    // head. A file this size spans several 1 KiB blocks, so this exercises the
    // block map rather than just the first pointer.
    t.check(
        "fs: probe.txt ends with its last line",
        text.ends_with(b"line 199 padding padding padding\n"),
    );

    // A read at an offset, which is the operation the ELF loader will make.
    let mut mid = [0u8; 32];
    let got = with_root(|fs| fs.read_at("/probe.txt", 23, &mut mid).ok()).flatten();
    t.check_eq("fs: read_at returns the requested length", got.unwrap_or(0) as u64, 32);
    t.check(
        "fs: read_at lands at the right offset",
        mid.starts_with(b"line 000 padding"),
    );

    // The file the loader is about to run.
    let Some(elf) = read_file("/bin/hello") else {
        t.check("fs: read /bin/hello", false);
        return;
    };
    t.check("fs: read /bin/hello", true);
    t.check("fs: /bin/hello is an ELF", elf.starts_with(&[0x7f, b'E', b'L', b'F']));
    t.note("fs: /bin/hello size", elf.len() as u64);

    // A path that does not exist must fail rather than return something.
    t.check(
        "fs: a missing path is an error",
        with_root(|fs| fs.read_file("/nope").is_err()).unwrap_or(false),
    );
}

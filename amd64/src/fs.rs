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
//! # The mount table
//!
//! Paths resolve through `akuma-vfs`'s [`MountTable`] (`MountSet<8>`), the same
//! type the AArch64 kernel's VFS uses, rather than through a single hard-wired
//! root. Today it holds exactly one mount — `/` — so the *resolution* it adds is
//! not yet load-bearing. Three other things it gives are:
//!
//! - **`/proc/mounts`**, rendered from the table in [`render_mounts`]. `busybox
//!   df` reads that file first and prints nothing without it, so a mount this
//!   kernel could not name was a mount `df` could not report.
//! - **`statfs`/`fstatfs` per mount** rather than per kernel: the numbers come
//!   from the `Filesystem` serving that path, so a second mount reports its own
//!   free space instead of the root's.
//! - **A second mount is now two lines**, at the point where there is one to
//!   make — which is where a mount table stops being ceremony.
//!
//! What is still absent is a path *namespace*: no per-process root, no bind
//! mounts, no container visibility rules. That is `akuma-isolation`'s
//! `Namespace`, which the AArch64 kernel layers on top of the same table
//! (`akuma-vfs-glue`), and this target has no boxes to need it.
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
use akuma_vfs::{
    DirEntry, Filesystem, FsError, FsStats, Metadata, MountSnapshot, MountTable,
};
use alloc::string::String;
use alloc::sync::Arc;
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

/// A partition on the USB disk, as something `akuma-ext2` can read.
///
/// The block device is the whole disk (`akuma_xhci` addresses it by absolute
/// LBA); this adds the partition's byte offset so `read_bytes(0)` lands at the
/// filesystem's start. On the reference box that is `sda1` at LBA 2048.
pub struct UsbDisk {
    partition_offset: u64,
}

impl UsbDisk {
    #[must_use]
    pub fn new(partition_offset: u64) -> Self {
        Self { partition_offset }
    }
}

impl BlockDevice for UsbDisk {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        crate::xhci::read_bytes(self.partition_offset + offset, buf).map_err(|_| ())
    }

    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
        crate::xhci::write_bytes(self.partition_offset + offset, data).map_err(|_| ())
    }
}

/// Whatever the root filesystem is sitting on.
///
/// Three things can be, and they arrive by different routes: a virtio-blk disk
/// from a VMM, a span of RAM that GRUB filled with an ext2 image before handing
/// over, or a partition on the USB disk this target's `xhci` driver brought up.
/// An enum rather than a `dyn` object because `Ext2Filesystem` is generic over
/// its device and the set is closed and known at compile time -- a trait object
/// would cost a vtable dispatch per block read to express a choice made once at
/// boot.
pub enum RootDevice {
    /// A virtio-blk disk: `vda`, from a VMM.
    Virtio(VirtioBlk),
    /// An ext2 image already in memory, placed there by the boot loader.
    Ram(crate::ramdisk::RamDisk),
    /// A partition on the USB disk (`sda1`), the persistent root.
    Usb(UsbDisk),
}

impl BlockDevice for RootDevice {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        match self {
            Self::Virtio(d) => d.read_bytes(offset, buf),
            Self::Ram(d) => d.read_bytes(offset, buf),
            Self::Usb(d) => d.read_bytes(offset, buf),
        }
    }

    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
        match self {
            Self::Virtio(d) => d.write_bytes(offset, data),
            Self::Ram(d) => d.write_bytes(offset, data),
            Self::Usb(d) => d.write_bytes(offset, data),
        }
    }
}

/// The kernel's mount table.
///
/// `Spinlock<Option<..>>` rather than a `OnceCell` for the reason the single
/// root had before it: mounting can fail (no disk, not ext2, a corrupt
/// superblock) and the kernel must boot anyway, so "nothing mounted" has to be
/// a representable state rather than a panic. `Option` and not a bare
/// `MountTable` because `MountSet::new` allocates its `Vec` and so cannot be a
/// `const` initialiser for a `static`.
static MOUNTS: Spinlock<Option<MountTable>> = Spinlock::new(None);

/// Wall-clock source for inode timestamps, in seconds since the Unix epoch.
///
/// `clock::now_us` is fed by the SNTP client that self-heals in the netpoll
/// daemon (`docs/archive/AKUMA_SELF_HEALING_PORT.md` § "The wall clock synced
/// once at boot or never"). It returns 0 until the first sync lands — which is
/// still the honest answer `Ext2Filesystem::new` documents `|| 0` for, not a
/// guess dressed up as one — so an early write before the clock sets is stamped
/// 1970 and every write after it is stamped correctly.
fn wall_clock_secs() -> u64 {
    crate::clock::now_us() / 1_000_000
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

/// Mount `device` at `/`, naming it in the diagnostics and in `/proc/mounts`.
///
/// `name` becomes the mount's **source** — the first column of `/proc/mounts`
/// and what `df` prints under `Filesystem`. It is the device the image came
/// from (`vda`, `sda1`, `module`), which is the only thing distinguishing the
/// three ways this target acquires a root.
///
/// The bare-metal path comes here with a [`RootDevice::Ram`]: an ext2 image the
/// boot loader left in memory, since a machine with no storage driver still
/// needs somewhere for `/bin/sh` to live.
pub fn mount_root_on(device: RootDevice, name: &str) -> bool {
    // Not an error worth halting for: a raw disk with no filesystem is a
    // legitimate thing to be handed, and the message says which happened.
    let Ok(fs) = Ext2Filesystem::new(device, wall_clock_secs) else {
        serial::puts("  fs:   ");
        serial::puts(name);
        serial::puts(" holds no readable ext2 image\n");
        return false;
    };
    let mut guard = MOUNTS.lock();
    let table = guard.get_or_insert_with(MountTable::new);
    // `mount_with` rather than `mount`: without a source the mount has no name
    // to print, and `/proc/mounts` with a `none` in column one is what `df`
    // shows under `Filesystem`.
    if let Err(e) = table.mount_with("/", Some(name), 0, Arc::new(fs)) {
        serial::puts("  fs:   could not mount ");
        serial::puts(name);
        serial::puts(" at /: ");
        serial::puts(fs_error_name(e));
        serial::puts("\n");
        return false;
    }
    drop(guard);
    serial::puts("  fs:   ext2 mounted on ");
    serial::puts(name);
    serial::puts("\n");
    true
}

/// A name for an `FsError`, for the console paths that cannot allocate.
const fn fs_error_name(e: FsError) -> &'static str {
    match e {
        FsError::NotFound => "not found",
        FsError::AlreadyExists => "already mounted",
        FsError::NoSpace => "mount table full",
        FsError::NotSupported => "not supported",
        FsError::NoFilesystem => "no filesystem",
        _ => "error",
    }
}

/// Resolve `path` through the mount table and run `f` against the filesystem
/// serving it, with the path rewritten relative to that mount point.
///
/// A closure rather than a returned reference for the reason `with_root` was
/// one: the table lives behind a lock and `MountSet::resolve` hands out a
/// borrow of an entry inside it, so a returned `&dyn Filesystem` would hand the
/// guard's lifetime to callers with no reason to think about it.
///
/// **The lock is held across the filesystem call**, and therefore across disk
/// I/O. That is deliberate and is exactly what the single `ROOT` lock this
/// replaced already did, so it is not a new hazard here — but it is the hazard
/// CLAUDE.md records for the AArch64 kernel (`MOUNT_TABLE` held across
/// filesystem calls, so a reaper taking that lock inverts against them). The
/// allocation-free alternative does not exist: `resolve_arc` clones the `Arc`
/// and drops the lock, at the cost of a `String` per call on every read. If
/// this target ever grows something that takes `MOUNTS` from a teardown path,
/// that is the trade to revisit.
fn with_fs<R>(path: &str, f: impl FnOnce(&dyn Filesystem, &str) -> R) -> Option<R> {
    let guard = MOUNTS.lock();
    let (fs, rel) = guard.as_ref()?.resolve(path)?;
    Some(f(fs, rel))
}

/// Run `f` against every mount, in table order.
///
/// The rows borrow out of the table and the lock is held for the whole visit,
/// so `f` must not allocate, block, or take another lock — the contract
/// `MountSet::for_each_mount` states. [`render_mounts`] is the intended use.
pub fn for_each_mount(f: impl FnMut(MountSnapshot<'_>)) {
    if let Some(table) = MOUNTS.lock().as_ref() {
        table.for_each_mount(f);
    }
}

/// How many filesystems are mounted.
#[must_use]
pub fn mount_count() -> usize {
    MOUNTS.lock().as_ref().map_or(0, MountTable::len)
}

/// `(filesystem name, statistics, mount flags)` for whichever mount serves
/// `path` — the body of `statfs`/`fstatfs`.
///
/// The name is copied rather than borrowed because it comes from
/// `Filesystem::name`, which borrows the filesystem, which borrows the table.
pub fn stats_for_path(path: &str) -> Result<(String, FsStats, u64), FsError> {
    let guard = MOUNTS.lock();
    let table = guard.as_ref().ok_or(FsError::NoFilesystem)?;
    let resolved = table.resolve_arc_full(path).ok_or(FsError::NotFound)?;
    let stats = resolved.fs.stats()?;
    Ok((String::from(resolved.fs.name()), stats, resolved.flags))
}

/// Render `/proc/mounts` into `buf`, returning the bytes written.
///
/// Fixed buffer and no allocation: this runs inside [`for_each_mount`], which
/// holds the table's spinlock. `busybox df` reads this file to learn what to
/// call `statfs` on, and prints nothing at all when it is missing — which is
/// what it did on this target until the mount table existed to render.
///
/// The format is Linux's: `source mountpoint fstype options 0 0`. Only the
/// `ro`/`rw` option is real; `df` and `mount` parse the column and ignore the
/// rest.
#[must_use]
pub fn render_mounts(buf: &mut [u8]) -> usize {
    use akuma_primitives::console::FmtBuf;
    use core::fmt::Write as _;

    let mut pos = 0usize;
    let mut w = FmtBuf { buf, pos: &mut pos };
    for_each_mount(|row| {
        let opts = if row.flags & akuma_vfs::MS_RDONLY != 0 { "ro" } else { "rw" };
        let _ = writeln!(
            w,
            "{} {} {} {},relatime 0 0",
            row.source.unwrap_or("none"),
            row.path,
            row.fs_type,
            opts
        );
    });
    pos.min(buf.len())
}

/// Read a whole file from the root filesystem.
#[must_use]
pub fn read_file(path: &str) -> Option<Vec<u8>> {
    with_fs(path, |fs, rel| fs.read_file(rel).ok())?
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
    with_fs(path, |fs, rel| fs.write_file(rel, data)).unwrap_or(Err(FsError::NoFilesystem))
}

/// Create a directory. `mkdirat(2)`'s body — the parent must already exist,
/// matching `akuma-ext2`'s (and `mkdir(2)`'s) own contract. First consumer:
/// `apk`'s cache-directory setup.
pub fn create_dir(path: &str) -> Result<(), FsError> {
    with_fs(path, |fs, rel| fs.create_dir(rel)).unwrap_or(Err(FsError::NoFilesystem))
}

/// Remove a file (or, with `rmdir`, an empty directory). `unlinkat(2)`'s body.
pub fn remove(path: &str, rmdir: bool) -> Result<(), FsError> {
    with_fs(path, |fs, rel| {
        if rmdir {
            fs.remove_dir(rel)
        } else {
            fs.remove_file(rel)
        }
    })
    .unwrap_or(Err(FsError::NoFilesystem))
}

/// Rename (move) a path. `renameat(2)`'s body — the target is replaced if it
/// exists, which is the atomic-tmpfile-swap shape `apk` names this syscall
/// for. First consumer: `apk`'s `.tmp.<pid>` + rename cache write.
pub fn rename(old_path: &str, new_path: &str) -> Result<(), FsError> {
    // Both paths must live on the same mount — a rename across filesystems is
    // `EXDEV`, which is why `mv` falls back to copy-and-unlink. Resolving each
    // separately and calling `rename` on the first would silently rename to a
    // path relative to the wrong mount, so the second is resolved against the
    // same table entry and refused when it lands elsewhere.
    let guard = MOUNTS.lock();
    let Some(table) = guard.as_ref() else {
        return Err(FsError::NoFilesystem);
    };
    let Some((fs, old_rel)) = table.resolve(old_path) else {
        return Err(FsError::NotFound);
    };
    let Some((new_fs, new_rel)) = table.resolve(new_path) else {
        return Err(FsError::NotFound);
    };
    if !core::ptr::eq(fs, new_fs) {
        return Err(FsError::NotSupported);
    }
    fs.rename(old_rel, new_rel)
}

/// Create a symlink. `symlinkat(2)`'s body. First consumer: `apk add` —
/// package contents carry symlinks (`.so.1` versioned-library names), and
/// every one of them failed with ENOSYS until this existed.
pub fn create_symlink(link_path: &str, target: &str) -> Result<(), FsError> {
    with_fs(link_path, |fs, rel| fs.create_symlink(rel, target))
        .unwrap_or(Err(FsError::NoFilesystem))
}

/// Read a symlink's target. `readlink(2)`'s body.
pub fn read_symlink(path: &str) -> Result<String, FsError> {
    with_fs(path, |fs, rel| fs.read_symlink(rel)).unwrap_or(Err(FsError::NoFilesystem))
}

/// Set file timestamps. `utimensat(2)`'s body; `None` leaves a stamp alone
/// (`UTIME_OMIT`), matching the VFS trait's contract. First consumer: `apk`'s
/// "preserve owner mtime" pass over extracted files.
pub fn set_times(path: &str, atime_secs: Option<u64>, mtime_secs: Option<u64>) -> Result<(), FsError> {
    with_fs(path, |fs, rel| fs.set_times(rel, atime_secs, mtime_secs))
        .unwrap_or(Err(FsError::NoFilesystem))
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
    with_fs(path, |fs, rel| fs.metadata(rel).ok())?
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
    with_fs(path, |fs, rel| fs.read_dir(rel).ok())?
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
    let names = with_fs("/", |fs, rel| {
        fs.read_dir(rel).map(|entries| {
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
    let got =
        with_fs("/probe.txt", |fs, rel| fs.read_at(rel, 23, &mut mid).ok()).flatten();
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
        with_fs("/nope", |fs, rel| fs.read_file(rel).is_err()).unwrap_or(false),
    );

    mount_table_smoke_test(t);
}

/// The mount table, `/proc/mounts` and `statfs` — the three things that stopped
/// being one hardcoded root.
///
/// Host tests cover `MountSet` itself (`akuma-vfs`); what they cannot show is
/// that *this kernel's* table was populated at boot and that the two consumers
/// read it. Each check below fails if the wiring is missing rather than if the
/// vocabulary is wrong.
fn mount_table_smoke_test(t: &mut Suite) {
    t.check_eq("mount: exactly one mount after boot", mount_count() as u64, 1);

    // Resolution, at the root and one level down. `resolve` rewrites the path
    // relative to the mount point, so a `/` mount must hand back the path
    // unchanged — get that wrong and every open silently addresses the wrong
    // name.
    let root_rel = with_fs("/bin/hello", |_, rel| String::from(rel));
    t.check(
        "mount: / resolves a path unchanged",
        root_rel.as_deref() == Some("/bin/hello"),
    );

    // `/proc/mounts`, exactly as `busybox df` will read it.
    let mut buf = [0u8; 1024];
    let n = render_mounts(&mut buf);
    let text = core::str::from_utf8(&buf[..n]).unwrap_or("");
    t.note("mount: /proc/mounts bytes", n as u64);
    t.check("mount: /proc/mounts has a line", n > 0 && text.ends_with('\n'));
    // Six space-separated columns is what `df` parses; five or seven and it
    // reads the wrong field or skips the line without saying so.
    let cols = text.lines().next().unwrap_or("").split(' ').count();
    t.check_eq("mount: /proc/mounts row has 6 columns", cols as u64, 6);
    t.check("mount: /proc/mounts names / as ext2", text.contains(" / ext2 "));
    // The source column is the device name `mount_root_on` was given, and it is
    // what `df` prints under `Filesystem`. `none` here means the mount was
    // recorded without one.
    t.check("mount: /proc/mounts names a source", !text.starts_with("none "));

    // `statfs`'s body. A filesystem reporting zero total blocks would give
    // `df` a 0-byte disk and a divide-by-zero Use%.
    match stats_for_path("/") {
        Ok((name, stats, _)) => {
            t.check("mount: statfs names the filesystem ext2", name == "ext2");
            t.check("mount: statfs reports a non-empty filesystem", stats.total_blocks > 0);
            t.check("mount: statfs block size is a power of two", stats.block_size.is_power_of_two());
            t.check("mount: statfs free <= total", stats.free_blocks <= stats.total_blocks);
            t.note("mount: total MiB", stats.total_bytes() / (1024 * 1024));
            t.note("mount: free MiB", stats.free_bytes() / (1024 * 1024));
        }
        Err(_) => {
            t.check("mount: statfs on / succeeds", false);
        }
    }

    // A path with no mount behind it must be an error, not the root's numbers.
    // Only reachable once there is a mount that is not `/`, so this pins the
    // direction rather than the behaviour: with a single `/` mount every
    // absolute path resolves, which is what the check asserts today.
    t.check("mount: every absolute path resolves under a / mount", stats_for_path("/anything").is_ok());
}

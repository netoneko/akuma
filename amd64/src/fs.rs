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
//! No writes are exercised. `Filesystem::write_file` exists and the block driver
//! can write, but a self-test that mutates the image would make the image
//! stateful across boots — the next run would start from whatever the last one
//! left. The read path is what the ELF loader needs.

use akuma_ext2::{BlockDevice, Ext2Filesystem};
use akuma_selftest::Suite;
use akuma_vfs::{Filesystem, Metadata};
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

/// The mounted root filesystem.
///
/// A `Spinlock<Option<..>>` rather than a `OnceCell`: mounting can fail (no
/// disk, not ext2, a corrupt superblock) and the kernel must boot anyway, so
/// "not mounted" has to be a representable state rather than a panic.
static ROOT: Spinlock<Option<Ext2Filesystem<VirtioBlk>>> = Spinlock::new(None);

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
    // Not an error worth halting for: a raw disk with no filesystem is a
    // legitimate thing to be handed, and the message says which happened.
    let Ok(fs) = Ext2Filesystem::new(VirtioBlk, no_clock) else {
        serial::puts("  fs:   vda holds no readable ext2 image\n");
        return false;
    };
    serial::puts("  fs:   ext2 mounted on vda\n");
    *ROOT.lock() = Some(fs);
    true
}

/// Run `f` against the root filesystem, if one is mounted.
///
/// A closure rather than a returned reference: the filesystem lives behind a
/// lock, and handing out a borrow would mean handing out the guard's lifetime
/// to callers that have no reason to think about it.
pub fn with_root<R>(f: impl FnOnce(&Ext2Filesystem<VirtioBlk>) -> R) -> Option<R> {
    ROOT.lock().as_ref().map(f)
}

/// Read a whole file from the root filesystem.
#[must_use]
pub fn read_file(path: &str) -> Option<Vec<u8>> {
    with_root(|fs| fs.read_file(path).ok())?
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

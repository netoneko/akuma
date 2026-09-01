//! Kernel ext2 wrapper — bridges `akuma_ext2` to the kernel block devices.

use alloc::sync::{Arc, Weak};
use akuma_vfs::Filesystem;
use spinning_top::Spinlock;
pub use akuma_ext2::{BlockDevice, Ext2Filesystem};

/// The mounted ext2 filesystems, for the dead-thread lock sweep.
///
/// `akuma-locks-rw` owns no global registry on purpose: enumerating locks is the
/// business of whoever owns them (`docs/archive/AKUMA_EXT2_CLEANUP.md` §4.5). This is
/// that owner.
///
/// **Why not the VFS mount table**, which already enumerates every filesystem and has
/// the `sync_all` precedent: `MountTable::resolve` hands out a `&dyn Filesystem`
/// borrowed from the table, so its callers hold `src/vfs::MOUNT_TABLE` *across*
/// filesystem calls. A reaper that took that lock would invert against them — thread A
/// holds the mount table and blocks in ext2 on a lock the dead thread left held, while
/// the recycler blocks on the mount table trying to release it. Nothing would ever run
/// again. This lock is taken only to write a slot at mount and to read the slots at
/// reap; it never covers device I/O or a filesystem call, so it cannot be part of a
/// cycle.
///
/// `Weak`, not `Arc`, so a registration cannot pin an unmounted filesystem forever.
type Ext2Mount = Ext2Filesystem<KernelBlockDevice>;
const MAX_EXT2_MOUNTS: usize = 4;
static EXT2_MOUNTS: Spinlock<[Option<Weak<Ext2Mount>>; MAX_EXT2_MOUNTS]> =
    Spinlock::new([const { None }; MAX_EXT2_MOUNTS]);

/// Record a mount for the sweep, reusing a slot whose filesystem has been dropped.
///
/// A full table is not an error worth failing a mount over — it costs orphaned-lock
/// recovery on that one filesystem, and the waiter-side backstop still unblocks the
/// system — so it is reported and ignored.
fn register_for_reap(fs: &Arc<Ext2Mount>) {
    let mut slots = EXT2_MOUNTS.lock();
    for slot in slots.iter_mut() {
        if slot.as_ref().is_none_or(|w| w.strong_count() == 0) {
            *slot = Some(Arc::downgrade(fs));
            return;
        }
    }
    akuma_primitives::safe_print!(
        128,
        "[ext2] WARNING: more than {} ext2 mounts — orphaned-lock recovery is off for this one\n",
        MAX_EXT2_MOUNTS
    );
}

/// Release everything the dead thread `tid` held on any mounted ext2 filesystem.
///
/// Registered with `akuma_exec::threading::set_slot_reap_callback`, so it runs at the
/// TERMINATED→FREE transition where the tid is known dead and its slot cannot yet be
/// reissued — the contract `RecoverableRwLock::abandon_tid` is written against.
///
/// The upgraded handles are collected into a fixed array and the registry lock is
/// dropped **before** any of them is swept, so the sweep itself holds nothing.
pub fn reap_dead_thread(tid: usize) {
    let mut live: [Option<Arc<Ext2Mount>>; MAX_EXT2_MOUNTS] = [const { None }; MAX_EXT2_MOUNTS];
    {
        let slots = EXT2_MOUNTS.lock();
        for (out, slot) in live.iter_mut().zip(slots.iter()) {
            *out = slot.as_ref().and_then(Weak::upgrade);
        }
    }
    for fs in live.iter().flatten() {
        fs.abandon_tid(tid);
    }
}

/// Kernel block device adapter implementing `akuma_ext2::BlockDevice`.
///
/// Wraps one registered virtio-blk device. `idx` 0 is the boot disk
/// (`vda`); runtime `mount(2)` passes the index its `source` name resolved to.
pub struct KernelBlockDevice {
    pub idx: usize,
}

impl BlockDevice for KernelBlockDevice {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        akuma_virtio::block::read_bytes_at(self.idx, offset, buf).map_err(|_| ())
    }

    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
        akuma_virtio::block::write_bytes_at(self.idx, offset, data).map_err(|_| ())
    }
}

/// Mount ext2 from the boot device (`vda`), cache sized from the global cap.
pub fn mount() -> Result<Arc<dyn Filesystem>, akuma_vfs::FsError> {
    mount_device(0, None)
}

/// Mount ext2 from device `idx` with an optional per-instance cache cap.
///
/// Non-root instances pass `Some(cap)` so a second disk does not re-commit the
/// whole global cache budget (the cache never shrinks — `src/fs.rs` sizing
/// comments). Callers gate on `akuma_virtio::block::device_name(idx)` first.
pub fn mount_device(
    idx: usize,
    cache_cap: Option<usize>,
) -> Result<Arc<dyn Filesystem>, akuma_vfs::FsError> {
    let fs = Arc::new(Ext2Filesystem::new_with_cache_cap(
        KernelBlockDevice { idx },
        || crate::utc_time_us().unwrap_or(0),
        cache_cap,
    )?);
    // After construction (which does device I/O) and before the filesystem is
    // reachable — the registry lock must never cover either.
    register_for_reap(&fs);
    Ok(fs)
}

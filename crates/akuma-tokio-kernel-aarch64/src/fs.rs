//! The tokio surface and the root filesystem: ext2 on the first virtio-blk
//! device, mounted at `/` in `akuma-vfs-glue`'s shared mount table, plus
//! `/proc`. The amd64 half's `install_surface` / `disk_root` / `mount_root`,
//! with this machine's clock and RNG.

use alloc::sync::Arc;

use akuma_primitives::safe_print;
use akuma_vfs::Filesystem;

/// Microseconds since boot: the CNTV counter.
pub fn uptime_us() -> u64 {
    akuma_timer::uptime_us()
}

/// Fill `buf` from a SplitMix64 stream seeded by the counter. Not a CSPRNG:
/// virtio-rng is the AArch64 kernel's source and arrives with the devices
/// (`docs/archive/TAOXICHUAN_B21_AARCH64.md`); until then this is the amd64
/// half's no-RDRAND fallback, the same function.
pub fn rng_fill(buf: &mut [u8]) {
    static STATE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    let seed = STATE.fetch_add(0x9E37_79B9_7F4A_7C15, core::sync::atomic::Ordering::Relaxed)
        ^ akuma_cpu::sysreg::cntvct_el0_ordered();
    akuma_primitives::rng::splitmix_fill(seed, buf);
}

/// Install the tokio surface (glue's and `akuma-vfs-glue`'s hooks) and the
/// mount table. Before the root is mounted.
pub fn install_surface() {
    akuma_runtime_tokio::install(akuma_runtime_tokio::Kernel {
        utc_time_us: || akuma_primitives::clock::utc_time_us(uptime_us()),
        online_cpus: crate::smp::online_cpus,
        rng_fill: |b| {
            rng_fill(b);
            true
        },
        after_reap: || {
            let _ = akuma_tokio_kernel_core::proc::collect::<crate::mm::Aarch64>();
        },
    });
    {
        use akuma_syscalls_glue::{proc, version};
        let id = akuma_tokio_kernel_core::identity();
        proc::set_uts_identity(proc::UtsIdentity {
            sysname: proc::uts_join(&["Linux"]),
            nodename: proc::uts_join(&[id.nodename]),
            release: proc::uts_join(&[id.version, "-", version::GIT_SHA, "-", id.nodename]),
            version: proc::uts_join(&[version::BUILD_ID]),
        });
    }
    // A process `akuma-exec` ends (a fatal signal's default action inside glue)
    // must still reach this kernel's `wait4` (`proc::on_thread_terminated`).
    akuma_threading::set_slot_purge_callback(akuma_tokio_kernel_core::proc::on_thread_terminated::<crate::mm::Aarch64>);
    akuma_vfs_glue::init();
    akuma_threading::set_slot_reap_callback(akuma_vfs_glue::reap_dead_thread);
}

/// The virtio-blk device as `akuma-ext2` reads it: device 0, `vda`.
#[cfg(feature = "disk")]
struct Vda;

#[cfg(feature = "disk")]
impl akuma_ext2::BlockDevice for Vda {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        akuma_virtio::block::read_bytes(offset, buf).map_err(|_| ())
    }
    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
        akuma_virtio::block::write_bytes(offset, data).map_err(|_| ())
    }
}

/// The ext2 inode clock: UTC once anchored, else 0.
#[cfg(feature = "disk")]
fn ext2_clock() -> u64 {
    akuma_primitives::clock::utc_time_us(uptime_us()).unwrap_or(0)
}

/// The default root: ext2 on the first virtio-blk device.
#[cfg(feature = "disk")]
pub fn disk_root() -> Option<Arc<dyn Filesystem>> {
    if akuma_virtio::block::init().is_err() {
        safe_print!(128, "  [FATAL] no virtio block device -- the default root (ext2 on virtio-blk) cannot mount\n");
        return None;
    }
    let Ok(fs) = akuma_ext2::Ext2Filesystem::new(Vda, ext2_clock) else {
        safe_print!(96, "  fs:   /dev/vda holds no readable ext2 image\n");
        return None;
    };
    safe_print!(64, "  fs:   ext2 root on /dev/vda\n");
    Some(Arc::new(fs))
}

/// The mounted root, for the arms that read it directly (exec's image, the
/// lazy file pages). Set once by [`mount_root`], before EL0 exists.
static ROOT_FS: akuma_primitives::cell::BorrowCell<Option<&'static dyn Filesystem>> =
    akuma_primitives::cell::BorrowCell::new(None);

/// The mounted root. Panics before the mount, which precedes every EL0 crossing.
pub fn root_fs() -> &'static dyn Filesystem {
    ROOT_FS.get().expect("root_fs before the mount")
}

/// Mount `fs` at `/` and `/proc` beside it; arm the file-page cache and the
/// shared-page table. `false` when the root could not be mounted.
pub fn mount_root(fs: Arc<dyn Filesystem>) -> bool {
    if akuma_vfs_glue::mount_with("/", Some("/dev/vda"), 0, fs.clone()).is_err() {
        safe_print!(96, "  [FATAL] could not mount the root into the shared mount table\n");
        return false;
    }
    akuma_vfs_glue::fs::mark_initialized();
    #[cfg(feature = "disk")]
    akuma_ext2::init_inode_freed_hook(akuma_tokio_kernel_core::shmpages::inode_freed);
    akuma_fpcache::init(akuma_pmm::total_count() * 4096);
    akuma_tokio_kernel_core::shmpages::init();
    let proc_fs = Arc::new(akuma_vfs_glue::proc::ProcFilesystem::new());
    if akuma_vfs_glue::mount_with("/proc", Some("proc"), 0, proc_fs).is_err() {
        safe_print!(64, "  fs:   /proc mount failed\n");
    }
    // The mount lives for the rest of the boot: one `Arc` leaked to a `'static` borrow.
    let fs: &'static Arc<dyn Filesystem> = alloc::boxed::Box::leak(alloc::boxed::Box::new(fs));
    ROOT_FS.set(Some(&**fs));
    true
}

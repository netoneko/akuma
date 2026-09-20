//! Stage N: a filesystem — the block devices this target can boot from, and the
//! mount that puts one of them behind [`akuma_vfs_glue`]'s path walk.
//!
//! `akuma-ext2` mounted on a block device, so the kernel can open a file by
//! path. Like the block driver before it, the ext2 code is used **unmodified**
//! — it already built for `x86_64-unknown-none`, it already forbids `unsafe`,
//! and its whole interface to a disk is two methods:
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
//! # The mount table is not here any more (C1 step 4a, 2026-09-07)
//!
//! This module used to carry its own `static MOUNTS: Spinlock<Option<MountTable>>`
//! and a `with_fs` that resolved a path against it, plus twelve wrappers —
//! `read_file`, `write_file`, `create_dir`, `remove`, `rename`,
//! `create_symlink`, `read_symlink`, `set_times`, `metadata`, `read_dir`,
//! `stats_for_path`, `render_mounts` — whose bodies were one `with_fs` call
//! each. All of that is [`akuma_vfs_glue`] now, the same instance the AArch64
//! kernel mounts into and, more to the point, **the instance
//! `akuma-syscalls-glue::fs` resolves through**: a folded VFS syscall arm
//! cannot see a mount table this target keeps to itself, so replacing the
//! private one is step 4's prerequisite rather than a tidy-up
//! (`docs/archive/AKUMA_SELF_HOSTING_AMD64.md` § C1).
//!
//! What the swap *adds*, none of which was written here:
//!
//! - **The lock is no longer held across disk I/O.** The old `with_fs` handed a
//!   borrowed `&dyn Filesystem` out of the guard, so every read ran under the
//!   mount-table spinlock — the hazard CLAUDE.md records for the AArch64
//!   kernel. `resolve_mount` clones the `Arc` and drops the lock first.
//! - **A real path walk**: `..`, `.`, a trailing slash and the process CWD are
//!   normalised before resolution, and `resolve_symlinks` follows links in
//!   *interior* components. The old table saw whatever string `fd.rs` handed it.
//! - **Synthetic `/dev` and `/etc/mtab`**, resolve-time nodes rather than
//!   mounted filesystems. `/dev` did not exist on this target at all
//!   (`AKUMA_SELF_HOSTING_AMD64.md` open issue 2); it now *lists* and *stats*.
//!   Opening one for its bytes is still `fd.rs`'s job and is not wired — see
//!   [`smoke_test`]'s `dev:` checks, which pin exactly that boundary.
//! - **Read-only mounts are enforced** at every write chokepoint (`MS_RDONLY`
//!   → `FsError::ReadOnly` → `EROFS`), which the private table recorded and
//!   never consulted.
//!
//! What deliberately did **not** come with it is `/proc`. `akuma-vfs-glue`
//! carries a `ProcFilesystem`, and it renders from `akuma-exec`'s process
//! table — which this target does not populate, so mounting it would replace
//! `fd.rs`'s synthetic `/proc` (which reads *this* kernel's tables and works)
//! with one that reports an empty machine. It becomes the right move in the
//! same step that gives this target `akuma-exec` processes, not before.
//!
//! # What is still absent
//!
//! A path *namespace*: no per-process root, no bind mounts, no container
//! visibility rules. The machinery is present — `akuma_vfs_glue::resolve_mount`
//! consults `current_process_shared()?.namespace` first — but this target
//! registers no `akuma-exec` processes, so every resolution takes the
//! no-process fallback (`resolve_path("/", path)` against the global table),
//! which is exactly what the private table did. That fallback is the seam: the
//! day processes exist here, CWD and namespaces start working with no change to
//! this file.
//!
//! Writes are exercised (2026-09-04, `write_file`) — `fd::sys_write` on a file
//! opened `O_CREAT`/`O_WRONLY` buffers into the descriptor's own `Vec<u8>` and
//! flushes once, at `close(2)`. `fd::smoke_test` writes and reads one back as
//! part of the boot self-tests: the old worry about a mutating self-test making
//! the image stateful across boots does not apply here, because `run.sh`
//! rebuilds the image on every run — nothing depends on this image surviving to
//! the next boot unchanged.

use akuma_ext2::{BlockDevice, Ext2Filesystem};
#[cfg(not(feature = "no-tests"))]
use akuma_selftest::Suite;
use akuma_vfs::FsError;
use alloc::sync::Arc;

use crate::serial;

/// The synchronous filesystem facade, re-exported so `crate::fs::read_file` and
/// friends keep naming one thing.
///
/// The AArch64 binary does the same (`src/fs.rs` is `pub use
/// akuma_vfs_glue::fs::*`), so the two kernels call the identical functions.
/// These return `Result<_, FsError>` where this module's own wrappers returned
/// `Option`; the callers converted rather than the crate being re-wrapped,
/// because an `Option` throws away *which* error the VFS reported and
/// `EROFS`-vs-`ENOENT` is now a distinction this layer can make.
// `read_at` has only ever had one caller in this kernel — `fs::smoke_test` —
// so a `no-tests` build re-exports something nothing names. It stays in the
// list: this is the module's public VFS surface, and thinning it by which
// arms happen to be wired today is how a re-export set drifts from the thing
// it is re-exporting.
#[allow(unused_imports)]
pub use akuma_vfs_glue::{
    create_dir, create_symlink, exists, list_dir, metadata, read_at, read_at_open_file, read_file,
    read_symlink, remove_dir, remove_file, rename, resolve_symlinks, set_times, stats_for_path,
    write_file,
};

/// The exec-side image read.
///
/// `read_file` caps the kernel-side allocation at 16 MiB
/// (`akuma-ext2`'s `read_inode_data`, an OOM guard for a caller that has no
/// idea how big the file is) — and a real toolchain binary is bigger:
/// Alpine's `cc1` is **42 MB**, so every `gcc` run died as
/// `[execve] load failed: Read past end of image`-adjacent `ENOENT` before a
/// single byte of it was parsed. The exec path knows exactly what it is
/// reading and how big it is (`metadata` first), so it can afford what the
/// generic read cannot:
///
/// * the path is **symlink-resolved first** — `sys_execve` read the raw path,
///   so `cc` (a link to `gcc`) handed the loader the *link text* as the image
///   and `open`'s resolution never ran on this path;
/// * the bytes arrive through `read_at` in 64 KiB chunks, which never touches
///   the cap;
/// * the chunk buffer and the image are reserved **fallibly**
///   (`try_reserve_exact`) — under memory pressure this returns `Err` and the
///   exec fails with `ENOMEM`, rather than taking the kernel down in the
///   allocator. The chunk lives on the **heap**, deliberately: the kernel
///   stack is 32 KiB (`sched::STACK_SIZE`), so a 64 KiB `[0u8; CHUNK]` local
///   is a stack smash, not a buffer — the first boot with a stack array died
///   in `ClockBlockCache::get` with `cr2 = 0x8e`, several frames after the
///   overflowing write.
///
/// 256 MiB is the hard refusal: past it the load is a mistake, not a
/// binary. `cc1`'s 42 MB sits well inside, with room for `rustc`'s ~70 MB.
pub fn read_image(path: &str) -> Result<alloc::vec::Vec<u8>, akuma_vfs_glue::FsError> {
    use akuma_vfs_glue::FsError;
    const MAX_IMAGE: usize = 256 * 1024 * 1024;
    const CHUNK: usize = 64 * 1024;

    let resolved = resolve_symlinks(path);
    let size = metadata(&resolved)?.size as usize;
    if size > MAX_IMAGE {
        return Err(FsError::Internal);
    }
    let mut buf = alloc::vec::Vec::new();
    if buf.try_reserve_exact(size).is_err() {
        return Err(FsError::Internal);
    }
    let mut chunk: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    if chunk.try_reserve_exact(CHUNK).is_err() {
        return Err(FsError::Internal);
    }
    chunk.resize(CHUNK, 0);
    let mut off = 0;
    while off < size {
        let want = (size - off).min(CHUNK);
        let got = read_at(&resolved, off, &mut chunk[..want])?;
        if got == 0 {
            // The metadata said `size`; the file delivered less. A truncated
            // image is a different failure from a short one — refuse rather
            // than let the ELF loader find a zero tail.
            return Err(FsError::Internal);
        }
        buf.extend_from_slice(&chunk[..got]);
        off += got;
    }
    Ok(buf)
}

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

/// Wall-clock source for inode timestamps, in **microseconds** since the Unix
/// epoch — the unit `Ext2Filesystem::new` asks for.
///
/// `clock::now_us` is fed by the SNTP client that self-heals in the netpoll
/// daemon (`docs/archive/AKUMA_SELF_HEALING_PORT.md` § "The wall clock synced
/// once at boot or never"). It returns 0 until the first sync lands — which is
/// still the honest answer `Ext2Filesystem::new` documents `|| 0` for, not a
/// guess dressed up as one — so an early write before the clock sets is stamped
/// 1970 and every write after it is stamped correctly.
///
/// # It divided by a million twice, and that is why `cargo` rebuilt everything
///
/// This was `wall_clock_secs`, returning **seconds**, handed to a parameter
/// named `utc_time_us` whose consumer is `Ext2Filesystem::current_time`:
///
/// ```text
/// fn current_time(&self) -> u32 { ((self.time_fn)() / 1_000_000) as u32 }
/// ```
///
/// So every inode written by this kernel was stamped `epoch_seconds / 1e6` —
/// **1789** at the time of writing, i.e. 1970-01-01T00:29:49Z, and *constant*
/// for eleven and a half days at a stretch. A `u64` of seconds and a `u64` of
/// microseconds are the same type, so nothing could catch it but the units in
/// the two names, which disagreed.
///
/// The cost is `cargo`. Its fingerprinting is **mtime-based**: source files
/// staged from the host carry real 2026 mtimes, every artifact this kernel
/// writes carries 1970, so every output is older than its input and **nothing
/// is ever up to date**. A no-op rebuild recompiles the world, which on a
/// self-host loop is the difference between a build and a career.
/// `proposals/NEXT_AGENT_AMD64_SELFHOST_FIRST_BUILD.md` § 3 ranked the clock
/// first among the three things most likely to stop `cargo`, and named the
/// test that finds it: *does a file written now have a plausible mtime?*
///
/// The test that separates this from the other candidate (an mtime tracking
/// **uptime**, which is what 1789 also looks like on a box that has been up
/// 1789 seconds) is to write two files two minutes apart: uptime advances,
/// `epoch/1e6` does not.
fn wall_clock_us() -> u64 {
    crate::clock::now_us()
}

/// Point [`akuma_vfs_glue`] at this kernel's four facts and create the mount
/// table.
///
/// Called from `boot::install_shared_sinks` — i.e. from **both** entry points,
/// which is the arrangement C1 step 3 landed after the multiboot2 path was
/// found to be missing `exec_runtime::init` and died at the first folded
/// syscall (`AKUMA_SELF_HOSTING_AMD64.md` § "C1 step 3's first arm"). A
/// per-entry copy of this call is precisely the drift that cost.
///
/// Deliberately separate from [`mount_root_on`]: the synthetic `/dev` and
/// `/etc/mtab` nodes, and `list_dir` on an empty table, must answer on a
/// `DISK=none` boot too, and they need the table to exist and the hooks to be
/// installed even though nothing will ever be mounted into it.
pub fn init_vfs() {
    akuma_vfs_glue::set_hooks(akuma_vfs_glue::VfsGlueHooks {
        // No sound device on either amd64 rig, so `/proc/audio` reports none.
        // Not a `not_wired!`-style panic: the hook's whole contract is to
        // answer a yes/no about hardware, and "no" is the true answer here.
        audio_is_available: || false,
        // The crate's own front door, which is what the AArch64 binary passes
        // too (`crate::fs::exists` there *is* this function). The indirection
        // exists so `/proc` can probe paths without `akuma-vfs-glue` depending
        // on the binary; a self-reference is the degenerate case of that.
        fs_exists: exists,
        probed_core_count: crate::smp::online_cpus,
        // Microseconds, and `None` rather than `Some(0)` before the first SNTP
        // sync — `clock::now_us` returns 0 for "never synced", which as a
        // timestamp would be 1970 dressed up as a reading.
        utc_time_us: || crate::clock::is_synced().then(crate::clock::now_us),
        // The shared file-page cache, adopted 2026-09-13 with the demand-paged
        // file mappings that consult it (`mm::fill_file_pages`). Until then
        // every file mapping held its own copy of every page, which is what made
        // two concurrent `rustc`s read one `librustc_driver.so` twice and hold
        // it twice.
        //
        // `akuma_fpcache::init` directly rather than through a shim: the four
        // tunables it reads are `akuma-config` consts, and this target has no
        // second copy of them to supply.
        fpcache_init: akuma_fpcache::init,
        // `/proc/<pid>/maps` and `/proc/<pid>/statm`, which the shared
        // `ProcFilesystem` renders and cannot walk for itself — the leaf walk
        // is x86-only. See `fd::pid_map_rows`.
        pid_map_rows: crate::fd::pid_map_rows,
    });
    // Drop cached file pages when ext2 frees an inode number, **before** any
    // mapping can be made. The cache is keyed on `(inode, mount, offset)` and
    // ext2 reissues inode numbers, so without this a newly created file
    // silently inherits the cached pages of whatever last held its number —
    // a wrong-bytes bug, not a crash (`docs/archive/SELFHOST_ZERO_PAGE_HUNT.md`
    // §15). The AArch64 kernel registers it in `akuma_vfs_glue::fs::init`,
    // which this target does not call: it mounts ext2 itself. The other half of
    // invalidation — every mutating VFS entry point — is already shared code.
    akuma_ext2::init_inode_freed_hook(akuma_fpcache::invalidate_inode);

    // Size the cache from the RAM the PMM actually manages.
    //
    // The `fpcache_init` hook above is **not** what calls this: that hook fires
    // from `akuma_vfs_glue::fs::init`, which is the AArch64 binary's mount path
    // and not this one — this target mounts ext2 itself, a few lines below. It
    // is registered anyway so the two kernels answer the hook identically, and
    // the call is made here, at the equivalent moment.
    //
    // Guarded because this function is deliberately idempotent and called twice
    // (see `mount_root_on`), and `akuma_fpcache::init` is not: it re-arms and
    // re-announces itself, which printed the banner twice and would have read
    // as two caches.
    if akuma_fpcache::cap() == 0 {
        akuma_fpcache::init(akuma_pmm::total_count() * 4096);
    }

    // **The ext2 block cache's cap, which this target was never setting.**
    //
    // Same story as the `fpcache_init` call above and the same root cause —
    // `akuma_vfs_glue::fs::init` is the AArch64 mount path and this target
    // mounts ext2 itself — but this one was missed, so `CACHE_CAP_BYTES` kept
    // `akuma-ext2`'s `DEFAULT_CACHE_CAP_BYTES`: **16 MB**, a value whose own
    // doc comment says it is sized for `cargo test` ("large enough to exercise
    // eviction, small enough for host tests"). The AArch64 kernel runs
    // `min(RAM/8, FSCACHE_CEILING_MB)` — 384 MB on a box with the RAM for it.
    //
    // Invisible on anything with a small working set: a 2 MiB file fits in
    // 16 MB, so every read is a hit either way and no benchmark that stays
    // under the cap can see the difference. It is a build that pays — `rustc`
    // reading rlibs has a working set in the hundreds of megabytes, which is
    // the shape `docs/archive/BKL_RUSTC_SCALING_BASELINE.md` measured the
    // ceiling against in the first place.
    //
    // Same formula as the shared path, deliberately not a second policy: the
    // ceiling is `akuma_config::FSCACHE_CEILING_MB` and the RAM term is the
    // PMM's own count, so the two kernels size the cache identically.
    //
    // **Bounded by the kernel heap as well as by RAM**, which the shared formula
    // is not and does not need to be. On AArch64 the block cache is one tenant of
    // a heap that grows; here it lives inside a *fixed* `mem::HEAP_SIZE`, so
    // `min(RAM/8, ceiling)` alone let the cache claim 384 MB of 512 MB and left
    // the rest of the kernel 128 MB. That is fine until something asks for a big
    // contiguous block: `execve` reads the whole binary into the heap on this
    // target, and `rust-lld` — the linker the kernel's own build finishes with —
    // is **158 MB**. Measured 2026-09-18, an in-guest `cargo build -p akuma-amd64`
    // died at `[ALLOC FAIL] requested=65536 heap_total=512MB heap_used=510MB`
    // one allocation after `[HEAP] 505MB used (alloc=165777608 bytes)`.
    //
    // A quarter of the heap is the bound, so the cache can never be the reason a
    // large allocation has nowhere to go. It is deliberately *not* expressed as
    // "leave N bytes free": the thing to protect against is a single request the
    // size of a linker, and a fraction scales with the heap while a constant
    // would go stale the moment `HEAP_SIZE` moves.
    {
        let ram_bytes = akuma_pmm::total_count().saturating_mul(4096);
        let ceiling = akuma_config::FSCACHE_CEILING_MB * 1024 * 1024;
        // `mem::heap_size()`, not the `HEAP_SIZE` constant: the heap is sized
        // from RAM at boot now (1 GiB on a machine reporting >= 8 GiB), and the
        // constant is only the floor. Reading the constant here would silently
        // hold the cache at 128 MB on exactly the machines that can afford 256.
        let heap_share = crate::mem::heap_size() / 4;
        akuma_ext2::set_cache_cap_bytes(core::cmp::min(
            core::cmp::min(ram_bytes / 8, ceiling),
            heap_share,
        ));
    }

    akuma_vfs_glue::init();

    // 5b slice 3: mount the real `ProcFilesystem`.
    //
    // The reason it was not mounted is spent: it renders from `akuma-exec`'s
    // process table, and until 5b slices 1-2 this target registered nothing
    // there, so mounting it would have replaced a working synthetic `/proc`
    // with a report of an empty machine. Every process is registered now, so
    // it renders this kernel's real processes — and it renders them for **all**
    // pids, where `fd.rs` could only ever describe the running one.
    //
    // Mounted here rather than beside the ext2 root because `/proc` does not
    // depend on a disk: a `DISK=none` boot still has processes, and `ps` on it
    // should still work.
    //
    // `fd.rs` keeps three paths that this filesystem does not serve — see its
    // `/proc` comment — so the mount is additive rather than a swap.
    let proc_fs = alloc::sync::Arc::new(akuma_vfs_glue::proc::ProcFilesystem::new());
    // `AlreadyExists` is **not** a failure: this function runs twice on every
    // boot — once from `boot::install_shared_sinks` and once from
    // `mount_root_on`, which calls it to be safe on a path that may not have
    // booted through the shared sink — and the second call finds `/proc`
    // already mounted by the first.
    //
    // It printed `[FS] WARN: /proc mount failed; falling back to fd.rs's
    // synthetic view` for that, on **every rig**, which is a line that says
    // the opposite of what happened. It cost two wrong conclusions in one
    // session (2026-09-10): first that the metal's persistent root could not
    // mount procfs at all, then that the RAM-image rigs differed from it.
    // `/proc` mounts everywhere, at the first call.
    match akuma_vfs_glue::mount_with("/proc", Some("proc"), 0, proc_fs) {
        Ok(()) | Err(akuma_vfs::FsError::AlreadyExists) => {}
        Err(_) => {
            crate::serial::puts("[FS] WARN: /proc mount failed; falling back to fd.rs's synthetic view\n");
        }
    }
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
    mount_root_on(RootDevice::Virtio(VirtioBlk), "/dev/vda")
}

/// Install the two hooks that make a dead thread's ext2 locks recoverable.
///
/// The AArch64 kernel installs these in `akuma_vfs_glue::fs::init`, which this
/// target does not call (it mounts ext2 itself) — the same gap that left the
/// fpcache inode hook and the block-cache cap unset here until each was found
/// separately. Idempotent: both sinks are set-once.
///
/// **The mount itself needs no code here.** The sweep's registry is populated by
/// `akuma_vfs_glue::mount_with`, which both kernels already go through, and it
/// holds `Weak<dyn Filesystem>` — so it does not care that this target's root is
/// `Ext2Filesystem<RootDevice>` where the other's is
/// `Ext2Filesystem<KernelBlockDevice>`. What is left to wire is only *who to
/// tell*:
///
/// 1. **The reap.** `akuma-threading` reports a death; the shared sweep releases
///    what that thread held. Wired only now because until the slot reaper there
///    was no `TERMINATED → FREE` transition on this target to hang it off
///    (`docs/archive/AKUMA_AMD64_NO_SLOT_RECYCLER.md` §3.4).
/// 2. **The waiter-side backstop.** Any single waiter, alone, can drive the
///    sweep that frees it. This is the half that matters under load: the idle
///    loop's collector does not run while the box is busy, and a mount wedged
///    behind a dead writer is exactly the case where nothing is going idle.
pub fn install_reap_hooks() {
    akuma_threading::set_slot_reap_callback(akuma_vfs_glue::reap_dead_thread);
    akuma_locks_rw::register_backstop(|| {
        akuma_threading::x86_reap_terminated_slots();
    });
}

/// Mount `device` at `/`, naming it in the diagnostics and in `/proc/mounts`.
///
/// `name` becomes the mount's **source** — the first column of `/proc/mounts`
/// and what `df` prints under `Filesystem`. It is the device the image came
/// from (`/dev/vda`, `/dev/sda1`, `module`), which is the only thing
/// distinguishing the three ways this target acquires a root. The `/dev/`
/// prefix matters beyond cosmetics: `akuma_vfs_glue::device_is_mounted` strips
/// it to decide whether a raw block open would race the filesystem's own cache.
///
/// The bare-metal path comes here with a [`RootDevice::Ram`]: an ext2 image the
/// boot loader left in memory, since a machine with no storage driver still
/// needs somewhere for `/bin/sh` to live.
pub fn mount_root_on(device: RootDevice, name: &str) -> bool {
    // Idempotent (`akuma_vfs_glue::init` only fills an empty table, `set_hooks`
    // is a `OnceCopy`), and here as well as in `install_shared_sinks` because a
    // caller that reaches a mount without having booted through the shared
    // sink would otherwise get `FsError::NotInitialized` rather than a mount.
    init_vfs();

    // Not an error worth halting for: a raw disk with no filesystem is a
    // legitimate thing to be handed, and the message says which happened.
    let Ok(fs) = Ext2Filesystem::new(device, wall_clock_us) else {
        serial::puts("  fs:   ");
        serial::puts(name);
        serial::puts(" holds no readable ext2 image\n");
        return false;
    };
    // `mount_with` rather than `mount`: without a source the mount has no name
    // to print, and `/proc/mounts` with a `none` in column one is what `df`
    // shows under `Filesystem`. It is also what enrols this filesystem in the
    // orphaned-lock sweep — see `install_reap_hooks`.
    if let Err(e) = akuma_vfs_glue::mount_with("/", Some(name), 0, Arc::new(fs)) {
        serial::puts("  fs:   could not mount ");
        serial::puts(name);
        serial::puts(" at /: ");
        serial::puts(fs_error_name(e));
        serial::puts("\n");
        return false;
    }
    serial::puts("  fs:   ext2 mounted on ");
    serial::puts(name);
    serial::puts("\n");
    // The VFS answers now, so the `akuma_vfs_glue::fs` facade — the gate every
    // folded glue fs arm sits behind — may say so. `fs::init` is the AArch64
    // route to this state (virtio-blk check, its own mounts, the fpcache and
    // reap-hook wiring); this target arrived by its own, so it sets the flag
    // itself. Before this line, every folded `mkdirat`/`unlinkat`/`renameat`
    // answered `NotInitialized`, flattened to `EIO` — a working filesystem
    // reporting a hardware fault.
    akuma_vfs_glue::fs::mark_initialized();
    // After `mark_initialized`, not before: the reap sweeps mounted
    // filesystems, and there is nothing to recover a lock on until one answers.
    install_reap_hooks();
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
        FsError::NotInitialized => "mount table not initialised",
        _ => "error",
    }
}

/// Render `/proc/mounts` into `buf`, returning the bytes written.
///
/// The viewer is box 0 and there is no target process: this target registers no
/// `akuma-exec` processes, so there is no namespace to render *through* and the
/// global table is the whole answer. Both arguments become real in the step
/// that gives this kernel processes; passing them explicitly here is what makes
/// that a one-line change rather than a search.
#[must_use]
pub fn render_mounts(buf: &mut [u8]) -> usize {
    akuma_vfs_glue::render_mounts(0, None, buf)
}

/// How many filesystems are mounted, counted the way `df` counts them: rows in
/// `/proc/mounts`.
///
/// The mount table itself is `akuma-vfs-glue`'s private static and exposes no
/// length, deliberately — every consumer wants the *visible* set, which is a
/// function of the asking process's namespace rather than of the table. Reading
/// the rendered rows asks the question the callers actually have and exercises
/// the path `df` takes.
#[must_use]
pub fn mount_count() -> usize {
    let mut buf = [0u8; 1024];
    let n = render_mounts(&mut buf);
    // Not `bytecount` (which clippy suggests): eight mounts is the table's
    // ceiling, so this counts at most a few hundred bytes, once, at boot.
    #[allow(clippy::naive_bytecount)]
    buf[..n].iter().filter(|&&b| b == b'\n').count()
}

#[cfg(not(feature = "no-tests"))]
/// Mount, then prove the filesystem can be read.
pub fn smoke_test(t: &mut Suite, mounted: bool) {
    if !t.check("fs: ext2 mounted", mounted) {
        return;
    }

    // **The assertion that would have caught the bug this target actually had.**
    // Every hook for orphaned-lock recovery can be registered and the set of
    // filesystems to sweep still be empty — which is what happened here for as
    // long as the registry was typed to `Ext2Filesystem<KernelBlockDevice>` and
    // this kernel's root was `Ext2Filesystem<RootDevice>`. A sweep over zero
    // mounts is indistinguishable from a working one at every other observation
    // point, so it is checked from the mount side
    // (`docs/archive/AKUMA_AMD64_NO_SLOT_RECYCLER.md` §8.3).
    t.check(
        "fs: the root mount is enrolled for orphaned-lock recovery",
        akuma_vfs_glue::reapable_mount_count() >= 1,
    );

    // Directory listing first. It is the cheapest operation that proves the
    // inode table, the block groups and the directory-entry walk all agree — a
    // driver that could read a file by inode number but not resolve a name
    // would still fail here.
    let Ok(entries) = list_dir("/") else {
        t.check("fs: read_dir /", false);
        return;
    };
    let has_bin = entries.iter().any(|e| e.name == "bin");
    let has_probe = entries.iter().any(|e| e.name == "probe.txt");
    t.note("fs: entries in /", entries.len() as u64);
    t.check("fs: / contains bin/", has_bin);
    t.check("fs: / contains probe.txt", has_probe);

    // A file with known contents, checked byte by byte. `mkdisk.sh` writes a
    // header line then 200 numbered lines; a short read or a wrong block would
    // survive a length check and fail this.
    let Ok(text) = read_file("/probe.txt") else {
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
    let got = read_at("/probe.txt", 23, &mut mid).unwrap_or(0);
    t.check_eq("fs: read_at returns the requested length", got as u64, 32);
    t.check(
        "fs: read_at lands at the right offset",
        mid.starts_with(b"line 000 padding"),
    );

    // The file the loader is about to run.
    let Ok(elf) = read_file("/bin/hello") else {
        t.check("fs: read /bin/hello", false);
        return;
    };
    t.check("fs: read /bin/hello", true);
    t.check("fs: /bin/hello is an ELF", elf.starts_with(&[0x7f, b'E', b'L', b'F']));
    t.note("fs: /bin/hello size", elf.len() as u64);

    // A path that does not exist must fail rather than return something.
    t.check("fs: a missing path is an error", read_file("/nope").is_err());

    path_walk_smoke_test(t);
    dev_smoke_test(t);
    mount_table_smoke_test(t);
}

#[cfg(not(feature = "no-tests"))]
/// The path walk this target gained with `akuma-vfs-glue` (C1 step 4a).
///
/// The private mount table resolved whatever string it was handed. Every check
/// here failed before the swap and none of them is a property of ext2 — they
/// are the difference between a table lookup and a VFS, and each is something a
/// shell produces without being asked: `cd ..`, a trailing slash from tab
/// completion, `//` from string concatenation.
fn path_walk_smoke_test(t: &mut Suite) {
    // `..` and `.` collapse, so the same inode is reachable by more than one
    // spelling. This is also the check that replaces the old
    // "mount: / resolves a path unchanged": what mattered about that was that a
    // `/` mount does not corrupt the path on its way through, and a walk that
    // *rewrites* the path is a stronger version of the same question.
    t.check("path: .. collapses", read_file("/bin/../probe.txt").is_ok());
    t.check("path: . collapses", read_file("/./probe.txt").is_ok());
    t.check("path: a repeated separator collapses", read_file("//probe.txt").is_ok());
    // A trailing slash on a directory is legal and must not become a lookup of
    // an empty final component. `busybox` produces these constantly.
    t.check("path: a directory takes a trailing slash", list_dir("/bin/").is_ok());
    // `..` at the root is the root, not an error and not an escape.
    t.check("path: .. at the root stays at the root", read_file("/../probe.txt").is_ok());
    // Interior symlinks. `resolve_symlinks` is what `apk`'s `.so.1` chains and
    // every `/usr/bin -> bin` layout need; on an image with no symlink at all
    // this is the identity, which is still the answer the caller wants.
    t.check(
        "path: resolve_symlinks is the identity on a plain path",
        resolve_symlinks("/probe.txt") == "/probe.txt",
    );

    // A real link, made and removed here. `ln -s` worked on this target before
    // the swap and `cat` through the link did not: `readlinkat` called
    // `read_symlink` directly while `open` handed the link's own path to
    // `read_file`, which is `NotAFile` on a link inode. Both halves are checked
    // because the *first* is what already worked — a check that only proved the
    // link exists would have passed against the bug.
    //
    // The image is rebuilt on every local run and lives in RAM on the
    // bare-metal boot, but the USB root (`/dev/sda1`) persists, so the link is
    // removed again below rather than left as debris that a later boot's
    // `read_dir` count would trip over.
    const LINK: &str = "/tmp/.selftest-link";
    let _ = remove_file(LINK);
    match create_symlink(LINK, "/probe.txt") {
        Ok(()) => {
            t.check("path: readlink reports the target", read_symlink(LINK).as_deref() == Some("/probe.txt"));
            t.check(
                "path: resolve_symlinks follows a real link",
                resolve_symlinks(LINK) == "/probe.txt",
            );
            // The half that was broken: bytes through the link. `sys_openat`
            // runs the same `resolve_symlinks` before it reads.
            t.check(
                "path: a link reads the target's bytes",
                read_file(&resolve_symlinks(LINK))
                    .is_ok_and(|b| b.starts_with(b"AKUMA/amd64 ext2 probe\n")),
            );
            t.check("path: the test link is removed again", remove_file(LINK).is_ok());
        }
        Err(_) => {
            t.check("path: symlink creation succeeds", false);
        }
    }
}

#[cfg(not(feature = "no-tests"))]
/// The synthetic `/dev`, and exactly how far it goes.
///
/// `AKUMA_SELF_HOSTING_AMD64.md` open issue 2 is "`/dev` does not exist on this
/// target at all". Half of it closes here: the nodes now *exist* — `ls` lists
/// them and `stat` describes them — because that half is the VFS's and came
/// with the crate. The other half, serving a device's **bytes** from `open(2)`,
/// is `fd.rs`'s dispatch and is not wired, so the last check below asserts the
/// failure rather than leaving it undiscovered. Delete that check in the change
/// that wires `sys_openat`; if it starts failing on its own, something began
/// answering and this comment is stale.
fn dev_smoke_test(t: &mut Suite) {
    let listed = akuma_vfs_glue::list_dir("/dev").unwrap_or_default();
    let named = |n: &str| listed.iter().any(|e| e.name == n);
    t.check("dev: /dev lists null", named("null"));
    t.check("dev: /dev lists zero", named("zero"));
    t.check("dev: /dev lists urandom", named("urandom"));
    t.check("dev: /dev lists tty", named("tty"));

    // `stat` agrees with `ls`, which is the drift `DEVFS_MISSING.md` was
    // written about: on the AArch64 kernel these two answers came from
    // different copy-pasted lists and disagreed for months.
    match metadata("/dev/null") {
        Ok(m) => {
            t.check("dev: stat /dev/null is not a directory", !m.is_dir);
            // `S_IFCHR | 0666`. A wrong file type here makes a shell treat the
            // node as a regular file and try to truncate it.
            t.check_eq("dev: stat /dev/null mode", u64::from(m.mode), 0o020_666);
        }
        Err(_) => {
            t.check("dev: stat /dev/null succeeds", false);
        }
    }
    t.check("dev: /dev itself stats as a directory", metadata("/dev").is_ok_and(|m| m.is_dir));

    // The boundary, asserted so it cannot drift silently. Serving bytes is
    // `sys_openat`'s job on both kernels — the device table is deliberately
    // pure data (`akuma_vfs::dev`'s module header) — and this target has no
    // such arm yet.
    t.check("dev: reading a device node's bytes is still unwired", read_file("/dev/null").is_err());
}

#[cfg(not(feature = "no-tests"))]
/// The mount table, `/proc/mounts` and `statfs` — the three things that stopped
/// being one hardcoded root.
///
/// Host tests cover `MountSet` itself (`akuma-vfs`); what they cannot show is
/// that *this kernel's* table was populated at boot and that the two consumers
/// read it. Each check below fails if the wiring is missing rather than if the
/// vocabulary is wrong.
fn mount_table_smoke_test(t: &mut Suite) {
    // Two since 5b slice 3: the ext2 root and `/proc`. The count is asserted
    // rather than bounded because it is the cheapest statement of what this
    // kernel mounts, and a *third* appearing unannounced is exactly what this
    // check is for — it caught the `/proc` mount itself on the boot it landed.
    t.check_eq("mount: exactly two mounts after boot", mount_count() as u64, 2);
    // `/proc` must be one of them and must be a `proc`, not a second ext2: a
    // mount recorded with the wrong type still resolves paths and still lists
    // in `df`, and only the type column says which filesystem answered.
    {
        let mut b = [0u8; 1024];
        let n = render_mounts(&mut b);
        let rows = core::str::from_utf8(&b[..n]).unwrap_or("");
        t.check("mount: /proc is mounted as proc", rows.contains(" /proc proc "));
    }

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
    // `/etc/mtab` is the same bytes through the file API — the shape `mount(8)`
    // with no arguments reads, and a synthetic node rather than a file on the
    // image. It arrived with the crate; nothing on this target rendered it.
    t.check(
        "mount: /etc/mtab renders the same rows",
        read_file("/etc/mtab").is_ok_and(|rows| rows == buf[..n]),
    );

    // `statfs`'s body. A filesystem reporting zero total blocks would give
    // `df` a 0-byte disk and a divide-by-zero Use%.
    match stats_for_path("/") {
        Ok(view) => {
            t.check("mount: statfs names the filesystem ext2", view.fs_name == "ext2");
            t.check("mount: statfs reports a non-empty filesystem", view.stats.total_blocks > 0);
            t.check(
                "mount: statfs block size is a power of two",
                view.stats.block_size.is_power_of_two(),
            );
            t.check(
                "mount: statfs free <= total",
                view.stats.free_blocks <= view.stats.total_blocks,
            );
            // Mounted `flags = 0`, so the mount is writable and `f_flags` must
            // not carry `ST_RDONLY` — the bit `with_fs_write` now refuses on.
            t.check_eq("mount: statfs reports a writable mount", view.flags, 0);
            t.note("mount: total MiB", view.stats.total_bytes() / (1024 * 1024));
            t.note("mount: free MiB", view.stats.free_bytes() / (1024 * 1024));
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

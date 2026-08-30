//! Synchronous Filesystem API
//!
//! Provides a synchronous filesystem API that delegates to the VFS layer.
//! This module maintains backward compatibility with the original FAT32-based API.

use alloc::vec::Vec;
use spinning_top::Spinlock;

use crate::console;
use crate::vfs;

// Re-export types from VFS for backward compatibility
pub use crate::vfs::{DirEntry, FsError};

// ============================================================================
// Filesystem Statistics (backward compatible wrapper)
// ============================================================================




// ============================================================================
// Filesystem State
// ============================================================================

static FS_INITIALIZED: Spinlock<bool> = Spinlock::new(false);

// ============================================================================
// Public API
// ============================================================================

/// Initialize the filesystem
pub fn init() -> Result<(), FsError> {
    log("[FS] Initializing filesystem...\n");

    if !crate::block::is_initialized() {
        log("[FS] Error: Block device not initialized\n");
        return Err(FsError::BlockDeviceNotInitialized);
    }

    // Initialize VFS subsystem
    vfs::init();

    // Orphaned-lock recovery, rebuilt 2026-08-31 (`docs/archive/AKUMA_EXT2_CLEANUP.md`
    // §4). This was `akuma_ext2::init_thread_hooks(current_thread_id,
    // is_thread_terminated)`: ext2 recorded the write-lock owner's tid and, every 10 000
    // spins, *asked* whether that tid was still alive so it could force-unlock. The
    // question is unanswerable — a recycled tid makes it read a **new** occupant's
    // liveness, so on a busy system recovery simply never fired (§4.2a) — and
    // `force_unlock_write` is an unconditional store whose "no guard exists" contract no
    // crate can check.
    //
    // Now nobody asks. Two registrations replace it, and ext2 names no tid at all:
    //
    // 1. The runtime *reports* a death. `reap_dead_thread` runs at the TERMINATED→FREE
    //    transition, where the tid is genuinely dead and its slot cannot yet be
    //    reissued, and performs the same CAS-guarded release a live holder performs.
    // 2. The waiter-side backstop keeps the property the 10 000-spin poll actually
    //    provided: any waiter, alone, can unblock the system when a reap is late.
    //    `reclaim_terminated_slots` is the collector-independent recycler pass (it
    //    exists because thread 0's idle loop does not run while the system is busy —
    //    `BKL_VFS_CARVE_OUT.md` §11.4), so a blocked waiter drives the very sweep that
    //    frees it. Unregistered it degrades to a plain spin, which is why host tests and
    //    early boot need no hook.
    akuma_exec::threading::set_slot_reap_callback(crate::vfs::ext2::reap_dead_thread);
    akuma_locks_rw::register_backstop(|| {
        akuma_exec::threading::reclaim_terminated_slots();
    });

    // Drop `file_page_cache` entries when an inode number is reissued. The cache
    // is keyed on `(inode, file_offset)`, so without this a new file silently
    // inherits the cached pages of whatever last held its number — see
    // `akuma_ext2`'s `InodeFreedHook` and
    // `docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` §15.
    akuma_ext2::init_inode_freed_hook(crate::file_page_cache::invalidate_inode);

    // Size the ext2 block cache from detected RAM before mounting (the cache is
    // allocated in Ext2Filesystem::new). Cap at min(12.5% RAM, 384 MB): enough to
    // keep the read-only toolchain's hot pages resident across the many
    // rustc/cc/ld spawns, bounded so it can't starve user pages.
    // No-op unless the kernel is built with the `fs-cache` feature.
    //
    // The ceiling was 128 MB from 2026-08-02 to 2026-08-05. That was too small:
    // it was sized against `rustc --version` (~71 distinct 1 MB windows), which
    // is not the workload. A *real* rustc compile touches far more —
    // librustc_driver.so alone is 295 MB and rust-lld is 154 MB — so at 128 MB
    // the cache sat pegged full and evicted blocks before they were reused.
    // Measured 2026-08-05, 8 sequential in-VM `rustc -O` compiles at
    // MEMORY=4096, one disk-image clone per arm, `[FSCACHE]` from PSTATS:
    //
    //   cap     s/compile (warm)   misses    slots        heap    HEAP-GROW
    //   128 MB      10.72            701 822  32 768 full  256 MB  none
    //   256 MB      10.79            155 416  65 536 full  267 MB  none
    //   384 MB       9.03            226 605  98 304 full  398 MB  none
    //   512 MB       8.91            206 961 131 072 full  528 MB  total=512MB
    //
    // The response is a step, not a slope: 256 MB buys nothing measurable, 384 MB
    // buys 15.8%, and the extra 128 MB on top of that buys only 1.1 point more
    // while pushing the heap over the 512 MB boundary. 384 MB is that knee.
    //
    // Why this does not re-run the 2026-08-02 regression (heap 1152 MB,
    // `claimed=131074 pages`, PMM 908 518 -> 678 073 never returned, sshd
    // resetting at key exchange): that blowup was the contiguous `Vec` doubling a
    // 256 MB buffer to 512 MB with both live, and it was fixed by chunking the
    // backing store (CACHE_CHUNK_BYTES, akuma-ext2). The largest claim observed
    // at 512 MB here is `claimed=258 pages`, and sshd completed key exchange
    // after every arm. Still, the cache never shrinks — whatever it fills is
    // committed for the boot's lifetime, which is why the ceiling stays well
    // under the RAM/8 term rather than chasing the working set (>512 MB: even
    // the 512 MB arm ran full).
    //
    // See docs/archive/BKL_RUSTC_SCALING_BASELINE.md.
    {
        const PAGE: usize = 4096;
        const CACHE_CEILING: usize = 384 * 1024 * 1024;
        let ram_bytes = crate::pmm::total_count().saturating_mul(PAGE);
        let cap = core::cmp::min(ram_bytes / 8, CACHE_CEILING);
        akuma_ext2::set_cache_cap_bytes(cap);
        // Sized from the same RAM figure, but a *different* kind of consumer: this
        // one dedupes frames that would otherwise exist per-process anyway.
        crate::file_page_cache::init(ram_bytes);
    }

    // Mount ext2 filesystem at root
    let ext2_fs = vfs::ext2::mount()?;
    vfs::mount_with("/", Some("/dev/vda"), 0, ext2_fs)?;

    log("[FS] Ext2 filesystem mounted at /\n");

    // Mount procfs at /proc
    let proc_fs = alloc::sync::Arc::new(vfs::proc::ProcFilesystem::new());
    vfs::mount_with("/proc", Some("proc"), 0, proc_fs)?;

    log("[FS] Procfs mounted at /proc\n");

    // Verify by listing root directory
    match vfs::list_dir("/") {
        Ok(entries) => {
            log("[FS] Root directory accessible\n");
            log("[FS] Files in root: ");
            crate::safe_print!(32, "{}\n", entries.len());
        }
        Err(e) => {
            log("[FS] Failed to list root directory: ");
            crate::safe_print!(32, "{}\n", e);
            return Err(e);
        }
    }

    *FS_INITIALIZED.lock() = true;
    log("[FS] Filesystem initialized\n");
    Ok(())
}

/// Check if filesystem is initialized
pub fn is_initialized() -> bool {
    *FS_INITIALIZED.lock()
}

/// List directory contents
pub fn list_dir(path: &str) -> Result<Vec<DirEntry>, FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }
    vfs::list_dir(path)
}

/// Read entire file contents as bytes
pub fn read_file(path: &str) -> Result<Vec<u8>, FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }
    // Gated 2026-08-30. The filter was `path.contains("git")` — meant to trace the
    // `git` binary during a specific investigation, but "git" is a substring of
    // "github", so in a devbox whose source tree lives under
    // `/src/github.com/<user>/<repo>` it matched EVERY file read of the build,
    // including the whole `target/` tree. At `safe_print!(128, ...)` those paths
    // also overrun the buffer, so the lines truncate mid-path and lose their
    // newline — which is what makes them run into the next line in the log.
    // `SYSCALL_DEBUG_IO_ENABLED` is the existing flag for exactly this ("verbose
    // file I/O logging"); no substring test replaces it, because a filter that
    // silently widens is worse than a flag you have to turn on.
    match vfs::read_file(path) {
        Ok(data) => {
            if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                crate::safe_print!(128, "[FS] read_file(\"{}\") -> {} bytes\n", path, data.len());
            }
            Ok(data)
        },
        Err(e) => {
            if crate::config::SYSCALL_DEBUG_IO_ENABLED {
                crate::safe_print!(128, "[FS] read_file(\"{}\") -> Error: {}\n", path, e);
            }
            Err(e)
        }
    }
}


/// Write data to a file (creates or truncates)
pub fn write_file(path: &str, data: &[u8]) -> Result<(), FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }
    vfs::write_file(path, data)
}

/// Read data from a specific offset within a file
pub fn read_at(path: &str, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }
    vfs::read_at(path, offset, buf)
}

/// Read for an open file description — by the inode `open(2)` resolved, when it
/// resolved one. See [`vfs::read_at_open_file`].
pub fn read_at_open_file(path: &str, mount_id: u32, inode: u32, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }
    vfs::read_at_open_file(path, mount_id, inode, offset, buf)
}

/// `stat` for an open file description — by the inode `open(2)` resolved, when
/// it resolved one. See [`vfs::metadata_open_file`].
pub fn metadata_open_file(path: &str, mount_id: u32, inode: u32) -> Result<crate::vfs::Metadata, FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }
    vfs::metadata_open_file(path, mount_id, inode)
}

/// Write data at a specific offset within a file
pub fn write_at(path: &str, offset: usize, data: &[u8]) -> Result<usize, FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }
    vfs::write_at(path, offset, data)
}


/// Create a directory
pub fn create_dir(path: &str) -> Result<(), FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }
    vfs::create_dir(path)
}

/// Remove a file
pub fn remove_file(path: &str) -> Result<(), FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }
    vfs::remove_file(path)
}

/// Remove a directory
pub fn remove_dir(path: &str) -> Result<(), FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }
    vfs::remove_dir(path)
}

/// Rename/move a file or directory
pub fn rename(old_path: &str, new_path: &str) -> Result<(), FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }
    vfs::rename(old_path, new_path)
}

/// Check if a file or directory exists
pub fn exists(path: &str) -> bool {
    if !is_initialized() {
        return false;
    }
    vfs::exists(path)
}

/// Get file size
pub fn file_size(path: &str) -> Result<u64, FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }
    vfs::file_size(path)
}


// ============================================================================
// Logging
// ============================================================================

fn log(msg: &str) {
    console::print(msg);
}

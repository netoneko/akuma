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

    // Initialize ext2 thread hooks for orphaned lock recovery
    // These hooks allow the filesystem to detect when a thread holding the lock has died
    // and force-unlock to prevent permanent deadlock.
    unsafe {
        akuma_ext2::init_thread_hooks(
            akuma_exec::threading::current_thread_id,
            akuma_exec::threading::is_thread_terminated,
        );
    }
    
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
    vfs::mount("/", ext2_fs)?;

    log("[FS] Ext2 filesystem mounted at /\n");

    // Mount procfs at /proc
    let proc_fs = alloc::sync::Arc::new(vfs::proc::ProcFilesystem::new());
    vfs::mount("/proc", proc_fs)?;

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
    match vfs::read_file(path) {
        Ok(data) => {
            if path.contains("git") {
                crate::safe_print!(128, "[FS] read_file(\"{}\") -> {} bytes\n", path, data.len());
            }
            Ok(data)
        },
        Err(e) => {
             if path.contains("git") {
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

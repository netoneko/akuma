//! Synchronous Filesystem API
//!
//! Provides a synchronous filesystem API that delegates to the VFS layer.
//! This module maintains backward compatibility with the original FAT32-based API.

use alloc::string::String;
use alloc::vec::Vec;
use spinning_top::Spinlock;

use crate::console;
use crate::vfs;

// Re-export types from VFS for backward compatibility
pub use crate::vfs::{DirEntry, FsError};

// ============================================================================
// Open Mode (for async_fs compatibility)
// ============================================================================

/// File open mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    Read,
    Write,
    Append,
    ReadWrite,
}

// ============================================================================
// Filesystem Statistics (backward compatible wrapper)
// ============================================================================

/// Filesystem statistics
#[derive(Debug, Clone)]
pub struct FsStats {
    pub cluster_size: u32,
    pub total_clusters: u32,
    pub free_clusters: u32,
}

impl FsStats {
    pub fn total_bytes(&self) -> u64 {
        self.total_clusters as u64 * self.cluster_size as u64
    }

    pub fn free_bytes(&self) -> u64 {
        self.free_clusters as u64 * self.cluster_size as u64
    }

    pub fn used_bytes(&self) -> u64 {
        self.total_bytes() - self.free_bytes()
    }
}

impl From<vfs::FsStats> for FsStats {
    fn from(stats: vfs::FsStats) -> Self {
        Self {
            cluster_size: stats.block_size,
            total_clusters: stats.total_blocks as u32,
            free_clusters: stats.free_blocks as u32,
        }
    }
}

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

    // Mount ext2 filesystem at root
    let ext2_fs = vfs::ext2::Ext2Filesystem::mount()?;
    vfs::mount("/", ext2_fs)?;

    log("[FS] Ext2 filesystem mounted at /\n");

    // Mount procfs at /proc
    let proc_fs = alloc::boxed::Box::new(vfs::proc::ProcFilesystem::new());
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
    vfs::read_file(path)
}

/// Read file contents as a string
pub fn read_to_string(path: &str) -> Result<String, FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }
    vfs::read_to_string(path)
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

/// Append data to a file
pub fn append_file(path: &str, data: &[u8]) -> Result<(), FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }
    vfs::append_file(path, data)
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

/// Get filesystem statistics
pub fn stats() -> Result<FsStats, FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }
    vfs::stats("/").map(|s| s.into())
}

// ============================================================================
// Logging
// ============================================================================

fn log(msg: &str) {
    console::print(msg);
}

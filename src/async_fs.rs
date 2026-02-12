//! Async Filesystem API
//!
//! Provides async wrappers around the synchronous filesystem API.
//! These are thin wrappers that yield once before calling the sync FS,
//! allowing other async tasks a chance to run.
//!
//! NOTE: No locking is needed here because:
//! 1. The underlying VFS already uses spinlocks for thread safety
//! 2. We disable preemption during async polls, so there's no concurrent access
//! 3. Using async mutexes with our no-op waker block_on would cause deadlocks

use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::fs;
pub use crate::fs::{DirEntry, FsError, FsStats, OpenMode};

// ============================================================================
// Yield Helper
// ============================================================================

/// A future that yields once to allow other tasks to run
struct YieldOnce {
    yielded: bool,
}

impl YieldOnce {
    fn new() -> Self {
        Self { yielded: false }
    }
}

impl Future for YieldOnce {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.yielded {
            Poll::Ready(())
        } else {
            self.yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// Yield to allow other async tasks to run
async fn yield_now() {
    YieldOnce::new().await
}

// ============================================================================
// Async File Operations
// ============================================================================

/// Async wrapper for listing directory contents
pub async fn list_dir(path: &str) -> Result<Vec<DirEntry>, FsError> {
    yield_now().await;
    fs::list_dir(path)
}

/// Async wrapper for reading entire file as bytes
pub async fn read_file(path: &str) -> Result<Vec<u8>, FsError> {
    yield_now().await;
    fs::read_file(path)
}

/// Async wrapper for reading file as string
pub async fn read_to_string(path: &str) -> Result<String, FsError> {
    yield_now().await;
    fs::read_to_string(path)
}

/// Async wrapper for writing to a file
pub async fn write_file(path: &str, data: &[u8]) -> Result<(), FsError> {
    yield_now().await;
    fs::write_file(path, data)
}

/// Async wrapper for appending to a file
pub async fn append_file(path: &str, data: &[u8]) -> Result<(), FsError> {
    yield_now().await;
    fs::append_file(path, data)
}

/// Async wrapper for creating a directory
pub async fn create_dir(path: &str) -> Result<(), FsError> {
    yield_now().await;
    fs::create_dir(path)
}

/// Async wrapper for removing a file
pub async fn remove_file(path: &str) -> Result<(), FsError> {
    yield_now().await;
    fs::remove_file(path)
}

/// Async wrapper for removing a directory
pub async fn remove_dir(path: &str) -> Result<(), FsError> {
    yield_now().await;
    fs::remove_dir(path)
}

/// Async wrapper for renaming a file or directory
pub async fn rename(old_path: &str, new_path: &str) -> Result<(), FsError> {
    yield_now().await;
    fs::rename(old_path, new_path)
}

/// Async wrapper for checking if path exists
pub async fn exists(path: &str) -> bool {
    yield_now().await;
    fs::exists(path)
}

/// Async wrapper for getting file size
pub async fn file_size(path: &str) -> Result<u64, FsError> {
    yield_now().await;
    fs::file_size(path)
}

/// Async wrapper for getting filesystem stats
pub async fn stats() -> Result<FsStats, FsError> {
    yield_now().await;
    fs::stats()
}

// ============================================================================
// AsyncFile - Stateful async file handle
// ============================================================================

/// Async file handle for more complex file operations
pub struct AsyncFile {
    path: String,
    mode: OpenMode,
    position: u64,
}

impl AsyncFile {
    /// Open a file with the specified mode
    pub async fn open(path: &str, mode: OpenMode) -> Result<Self, FsError> {
        yield_now().await;

        // Validate the file exists for read modes
        match mode {
            OpenMode::Read | OpenMode::ReadWrite => {
                if !fs::exists(path) {
                    return Err(FsError::NotFound);
                }
            }
            OpenMode::Write | OpenMode::Append => {
                // Create file if it doesn't exist
                if !fs::exists(path) {
                    fs::write_file(path, &[])?;
                }
            }
        }

        let position = match mode {
            OpenMode::Append => fs::file_size(path).unwrap_or(0),
            _ => 0,
        };

        Ok(Self {
            path: String::from(path),
            mode,
            position,
        })
    }

    /// Read data from the file at the current position
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, FsError> {
        if self.mode == OpenMode::Write {
            return Err(FsError::PermissionDenied);
        }

        yield_now().await;

        // Read entire file and extract the portion we need
        let data = fs::read_file(&self.path)?;

        if self.position >= data.len() as u64 {
            return Ok(0);
        }

        let start = self.position as usize;
        let available = data.len() - start;
        let to_read = core::cmp::min(buf.len(), available);

        buf[..to_read].copy_from_slice(&data[start..start + to_read]);
        self.position += to_read as u64;

        Ok(to_read)
    }

    /// Write data to the file at the current position
    pub async fn write(&mut self, data: &[u8]) -> Result<usize, FsError> {
        if self.mode == OpenMode::Read {
            return Err(FsError::PermissionDenied);
        }

        yield_now().await;

        match self.mode {
            OpenMode::Write => {
                // For write mode, we write from position 0 (truncate handled at open)
                if self.position == 0 {
                    fs::write_file(&self.path, data)?;
                } else {
                    // Read existing content, modify, write back
                    let mut existing = fs::read_file(&self.path).unwrap_or_default();
                    let pos = self.position as usize;

                    // Extend if necessary
                    if pos > existing.len() {
                        existing.resize(pos, 0);
                    }

                    // Insert/overwrite data
                    if pos + data.len() > existing.len() {
                        existing.resize(pos + data.len(), 0);
                    }
                    existing[pos..pos + data.len()].copy_from_slice(data);

                    fs::write_file(&self.path, &existing)?;
                }
            }
            OpenMode::Append => {
                fs::append_file(&self.path, data)?;
            }
            OpenMode::ReadWrite => {
                // Read existing content, modify, write back
                let mut existing = fs::read_file(&self.path).unwrap_or_default();
                let pos = self.position as usize;

                // Extend if necessary
                if pos > existing.len() {
                    existing.resize(pos, 0);
                }

                // Insert/overwrite data
                if pos + data.len() > existing.len() {
                    existing.resize(pos + data.len(), 0);
                }
                existing[pos..pos + data.len()].copy_from_slice(data);

                fs::write_file(&self.path, &existing)?;
            }
            OpenMode::Read => unreachable!(),
        }

        self.position += data.len() as u64;
        Ok(data.len())
    }

    /// Seek to a position in the file
    pub async fn seek(&mut self, pos: SeekFrom) -> Result<u64, FsError> {
        yield_now().await;

        let file_size = fs::file_size(&self.path).unwrap_or(0);

        let new_pos = match pos {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => file_size as i64 + offset,
            SeekFrom::Current(offset) => self.position as i64 + offset,
        };

        if new_pos < 0 {
            return Err(FsError::InvalidPath);
        }

        self.position = new_pos as u64;
        Ok(self.position)
    }

    /// Get current position
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Get the file path
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Flush any buffered data (no-op for our implementation)
    pub async fn flush(&mut self) -> Result<(), FsError> {
        // Our implementation writes immediately, so flush is a no-op
        Ok(())
    }

    /// Close the file
    pub async fn close(self) {
        // No-op - we don't hold any resources
    }
}

// ============================================================================
// SeekFrom
// ============================================================================

/// Seek position for file operations
#[derive(Debug, Clone, Copy)]
pub enum SeekFrom {
    /// Seek from the start of the file
    Start(u64),
    /// Seek from the end of the file
    End(i64),
    /// Seek from the current position
    Current(i64),
}

// ============================================================================
// Convenience Functions
// ============================================================================

/// Read entire file contents as a string (convenience wrapper)
pub async fn read_string(path: &str) -> Result<String, FsError> {
    read_to_string(path).await
}

/// Write a string to a file (convenience wrapper)
pub async fn write_string(path: &str, content: &str) -> Result<(), FsError> {
    write_file(path, content.as_bytes()).await
}

/// Append a string to a file (convenience wrapper)
pub async fn append_string(path: &str, content: &str) -> Result<(), FsError> {
    append_file(path, content.as_bytes()).await
}

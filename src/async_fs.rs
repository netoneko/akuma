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
pub use crate::fs::{DirEntry, FsError, FsStats};

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

/// Async wrapper for getting filesystem stats
pub async fn stats() -> Result<FsStats, FsError> {
    yield_now().await;
    fs::stats()
}

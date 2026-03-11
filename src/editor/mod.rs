//! Kernel adapter for the akuma-editor crate.
//!
//! Provides the `EditorFs` implementation backed by the kernel's async VFS
//! and re-exports the public editor API.

use alloc::string::String;

pub use akuma_editor::{TermSize, TermSizeProvider};

use crate::async_fs;

struct KernelFs;

impl akuma_editor::EditorFs for KernelFs {
    async fn read_to_string(&self, path: &str) -> Result<String, ()> {
        async_fs::read_to_string(path).await.map_err(|_| ())
    }

    async fn write_file(&self, path: &str, data: &[u8]) -> Result<(), ()> {
        async_fs::write_file(path, data).await.map_err(|_| ())
    }
}

/// Run the neko editor with the kernel filesystem backend.
pub async fn run<S: embedded_io_async::Read + embedded_io_async::Write + TermSizeProvider>(
    stream: &mut S,
    filepath: Option<&str>,
) -> Result<(), &'static str> {
    akuma_editor::run(stream, &KernelFs, filepath).await
}

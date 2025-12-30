//! Filesystem Commands
//!
//! Commands for filesystem operations: ls, cat, write, append, rm, mkdir, df

use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use crate::async_fs;
use crate::shell::{Command, ShellError};
use crate::ssh_crypto::split_first_word;

// ============================================================================
// Ls Command
// ============================================================================

/// Ls command - list directory contents
pub struct LsCommand;

impl Command for LsCommand {
    fn name(&self) -> &'static str {
        "ls"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["dir"]
    }
    fn description(&self) -> &'static str {
        "List directory contents"
    }
    fn usage(&self) -> &'static str {
        "ls [path]"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ShellError>> + 'a>> {
        Box::pin(async move {
            let mut response = Vec::new();
            let path = if args.is_empty() {
                "/"
            } else {
                core::str::from_utf8(args).unwrap_or("/")
            };

            if !crate::fs::is_initialized() {
                response.extend_from_slice(b"Error: Filesystem not initialized\r\n");
                return Ok(response);
            }

            match async_fs::list_dir(path).await {
                Ok(entries) => {
                    if entries.is_empty() {
                        return Ok(response);
                    }

                    let mut dirs: Vec<_> = entries.iter().filter(|e| e.is_dir).collect();
                    let mut files: Vec<_> = entries.iter().filter(|e| !e.is_dir).collect();

                    dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                    files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

                    const COLOR_DIR: &[u8] = b"\x1b[1;34m";
                    const COLOR_RESET: &[u8] = b"\x1b[0m";

                    for entry in dirs {
                        let name = entry.name.to_lowercase();
                        response.extend_from_slice(COLOR_DIR);
                        response.extend_from_slice(name.as_bytes());
                        response.extend_from_slice(b"/");
                        response.extend_from_slice(COLOR_RESET);
                        response.extend_from_slice(b"\r\n");
                    }

                    for entry in files {
                        let name = entry.name.to_lowercase();
                        response.extend_from_slice(name.as_bytes());
                        response.extend_from_slice(b"\r\n");
                    }
                }
                Err(e) => {
                    let msg = format!("Error listing directory: {}\r\n", e);
                    response.extend_from_slice(msg.as_bytes());
                }
            }
            Ok(response)
        })
    }
}

/// Static instance
pub static LS_CMD: LsCommand = LsCommand;

// ============================================================================
// Cat Command
// ============================================================================

/// Cat command - display file contents
pub struct CatCommand;

impl Command for CatCommand {
    fn name(&self) -> &'static str {
        "cat"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["read"]
    }
    fn description(&self) -> &'static str {
        "Display file contents"
    }
    fn usage(&self) -> &'static str {
        "cat <filename>"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ShellError>> + 'a>> {
        Box::pin(async move {
            let mut response = Vec::new();

            if args.is_empty() {
                response.extend_from_slice(b"Usage: cat <filename>\r\n");
                return Ok(response);
            }

            if !crate::fs::is_initialized() {
                response.extend_from_slice(b"Error: Filesystem not initialized\r\n");
                return Ok(response);
            }

            let path = core::str::from_utf8(args).unwrap_or("");
            match async_fs::read_to_string(path).await {
                Ok(content) => {
                    for line in content.split('\n') {
                        response.extend_from_slice(line.as_bytes());
                        response.extend_from_slice(b"\r\n");
                    }
                }
                Err(e) => {
                    let msg = format!("Error reading file: {}\r\n", e);
                    response.extend_from_slice(msg.as_bytes());
                }
            }
            Ok(response)
        })
    }
}

/// Static instance
pub static CAT_CMD: CatCommand = CatCommand;

// ============================================================================
// Write Command
// ============================================================================

/// Write command - write text to file
pub struct WriteCommand;

impl Command for WriteCommand {
    fn name(&self) -> &'static str {
        "write"
    }
    fn description(&self) -> &'static str {
        "Write text to file"
    }
    fn usage(&self) -> &'static str {
        "write <filename> <content>"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ShellError>> + 'a>> {
        Box::pin(async move {
            let mut response = Vec::new();

            if args.is_empty() {
                response.extend_from_slice(b"Usage: write <filename> <content>\r\n");
                return Ok(response);
            }

            if !crate::fs::is_initialized() {
                response.extend_from_slice(b"Error: Filesystem not initialized\r\n");
                return Ok(response);
            }

            let (filename, content) = split_first_word(args);
            if content.is_empty() {
                response.extend_from_slice(b"Usage: write <filename> <content>\r\n");
                return Ok(response);
            }

            let path = core::str::from_utf8(filename).unwrap_or("");
            match async_fs::write_file(path, content).await {
                Ok(()) => {
                    let msg = format!("Wrote {} bytes to {}\r\n", content.len(), path);
                    response.extend_from_slice(msg.as_bytes());
                }
                Err(e) => {
                    let msg = format!("Error writing file: {}\r\n", e);
                    response.extend_from_slice(msg.as_bytes());
                }
            }
            Ok(response)
        })
    }
}

/// Static instance
pub static WRITE_CMD: WriteCommand = WriteCommand;

// ============================================================================
// Append Command
// ============================================================================

/// Append command - append text to file
pub struct AppendCommand;

impl Command for AppendCommand {
    fn name(&self) -> &'static str {
        "append"
    }
    fn description(&self) -> &'static str {
        "Append text to file"
    }
    fn usage(&self) -> &'static str {
        "append <filename> <content>"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ShellError>> + 'a>> {
        Box::pin(async move {
            let mut response = Vec::new();

            if args.is_empty() {
                response.extend_from_slice(b"Usage: append <filename> <content>\r\n");
                return Ok(response);
            }

            if !crate::fs::is_initialized() {
                response.extend_from_slice(b"Error: Filesystem not initialized\r\n");
                return Ok(response);
            }

            let (filename, content) = split_first_word(args);
            if content.is_empty() {
                response.extend_from_slice(b"Usage: append <filename> <content>\r\n");
                return Ok(response);
            }

            let path = core::str::from_utf8(filename).unwrap_or("");
            match async_fs::append_file(path, content).await {
                Ok(()) => {
                    let msg = format!("Appended {} bytes to {}\r\n", content.len(), path);
                    response.extend_from_slice(msg.as_bytes());
                }
                Err(e) => {
                    let msg = format!("Error appending to file: {}\r\n", e);
                    response.extend_from_slice(msg.as_bytes());
                }
            }
            Ok(response)
        })
    }
}

/// Static instance
pub static APPEND_CMD: AppendCommand = AppendCommand;

// ============================================================================
// Rm Command
// ============================================================================

/// Rm command - remove file
pub struct RmCommand;

impl Command for RmCommand {
    fn name(&self) -> &'static str {
        "rm"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["del"]
    }
    fn description(&self) -> &'static str {
        "Remove file"
    }
    fn usage(&self) -> &'static str {
        "rm <filename>"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ShellError>> + 'a>> {
        Box::pin(async move {
            let mut response = Vec::new();

            if args.is_empty() {
                response.extend_from_slice(b"Usage: rm <filename>\r\n");
                return Ok(response);
            }

            if !crate::fs::is_initialized() {
                response.extend_from_slice(b"Error: Filesystem not initialized\r\n");
                return Ok(response);
            }

            let path = core::str::from_utf8(args).unwrap_or("");
            match async_fs::remove_file(path).await {
                Ok(()) => {
                    let msg = format!("Removed: {}\r\n", path);
                    response.extend_from_slice(msg.as_bytes());
                }
                Err(e) => {
                    let msg = format!("Error removing file: {}\r\n", e);
                    response.extend_from_slice(msg.as_bytes());
                }
            }
            Ok(response)
        })
    }
}

/// Static instance
pub static RM_CMD: RmCommand = RmCommand;

// ============================================================================
// Mkdir Command
// ============================================================================

/// Mkdir command - create directory
pub struct MkdirCommand;

impl Command for MkdirCommand {
    fn name(&self) -> &'static str {
        "mkdir"
    }
    fn description(&self) -> &'static str {
        "Create directory"
    }
    fn usage(&self) -> &'static str {
        "mkdir <dirname>"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ShellError>> + 'a>> {
        Box::pin(async move {
            let mut response = Vec::new();

            if args.is_empty() {
                response.extend_from_slice(b"Usage: mkdir <dirname>\r\n");
                return Ok(response);
            }

            if !crate::fs::is_initialized() {
                response.extend_from_slice(b"Error: Filesystem not initialized\r\n");
                return Ok(response);
            }

            let path = core::str::from_utf8(args).unwrap_or("");
            match async_fs::create_dir(path).await {
                Ok(()) => {
                    let msg = format!("Created directory: {}\r\n", path);
                    response.extend_from_slice(msg.as_bytes());
                }
                Err(e) => {
                    let msg = format!("Error creating directory: {}\r\n", e);
                    response.extend_from_slice(msg.as_bytes());
                }
            }
            Ok(response)
        })
    }
}

/// Static instance
pub static MKDIR_CMD: MkdirCommand = MkdirCommand;

// ============================================================================
// Df Command
// ============================================================================

/// Df command - show disk usage
pub struct DfCommand;

impl Command for DfCommand {
    fn name(&self) -> &'static str {
        "df"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["diskfree"]
    }
    fn description(&self) -> &'static str {
        "Show disk usage"
    }

    fn execute<'a>(
        &'a self,
        _args: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ShellError>> + 'a>> {
        Box::pin(async move {
            let mut response = Vec::new();

            if !crate::fs::is_initialized() {
                response.extend_from_slice(b"Error: Filesystem not initialized\r\n");
                return Ok(response);
            }

            match async_fs::stats().await {
                Ok(stats) => {
                    let total_kb = stats.total_bytes() / 1024;
                    let free_kb = stats.free_bytes() / 1024;
                    let used_kb = stats.used_bytes() / 1024;
                    let percent_used = if stats.total_bytes() > 0 {
                        (stats.used_bytes() * 100) / stats.total_bytes()
                    } else {
                        0
                    };
                    let info = format!(
                        "Filesystem Statistics:\r\n  Total:  {} KB\r\n  Used:   {} KB ({}%)\r\n  Free:   {} KB\r\n  Cluster size: {} bytes\r\n",
                        total_kb, used_kb, percent_used, free_kb, stats.cluster_size
                    );
                    response.extend_from_slice(info.as_bytes());
                }
                Err(e) => {
                    let msg = format!("Error getting filesystem stats: {}\r\n", e);
                    response.extend_from_slice(msg.as_bytes());
                }
            }
            Ok(response)
        })
    }
}

/// Static instance
pub static DF_CMD: DfCommand = DfCommand;

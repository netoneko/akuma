//! Filesystem Commands
//!
//! Commands for filesystem operations: ls, cat, write, append, rm, mkdir, df

use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use embedded_io_async::Write;

use crate::async_fs;
use crate::shell::{Command, ShellError, VecWriter};
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
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let path = if args.is_empty() {
                "/"
            } else {
                core::str::from_utf8(args).unwrap_or("/")
            };

            if !crate::fs::is_initialized() {
                let _ = stdout.write(b"Error: Filesystem not initialized\r\n").await;
                return Ok(());
            }

            match async_fs::list_dir(path).await {
                Ok(entries) => {
                    if entries.is_empty() {
                        return Ok(());
                    }

                    let mut dirs: Vec<_> = entries.iter().filter(|e| e.is_dir).collect();
                    let mut files: Vec<_> = entries.iter().filter(|e| !e.is_dir).collect();

                    dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                    files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

                    const COLOR_DIR: &[u8] = b"\x1b[1;34m";
                    const COLOR_RESET: &[u8] = b"\x1b[0m";

                    for entry in dirs {
                        let name = entry.name.to_lowercase();
                        let _ = stdout.write(COLOR_DIR).await;
                        let _ = stdout.write(name.as_bytes()).await;
                        let _ = stdout.write(b"/").await;
                        let _ = stdout.write(COLOR_RESET).await;
                        let _ = stdout.write(b"\r\n").await;
                    }

                    for entry in files {
                        let name = entry.name.to_lowercase();
                        let _ = stdout.write(name.as_bytes()).await;
                        let _ = stdout.write(b"\r\n").await;
                    }
                }
                Err(e) => {
                    let msg = format!("Error listing directory: {}\r\n", e);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
            }
            Ok(())
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
        stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            // If no args but stdin provided, just pass through stdin (useful for pipes)
            if args.is_empty() {
                if let Some(data) = stdin {
                    let _ = stdout.write(data).await;
                    return Ok(());
                }
                let _ = stdout.write(b"Usage: cat <filename>\r\n").await;
                return Ok(());
            }

            if !crate::fs::is_initialized() {
                let _ = stdout.write(b"Error: Filesystem not initialized\r\n").await;
                return Ok(());
            }

            let path = core::str::from_utf8(args).unwrap_or("");
            match async_fs::read_to_string(path).await {
                Ok(content) => {
                    for line in content.split('\n') {
                        let _ = stdout.write(line.as_bytes()).await;
                        let _ = stdout.write(b"\r\n").await;
                    }
                }
                Err(e) => {
                    let msg = format!("Error reading file: {}\r\n", e);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
            }
            Ok(())
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
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            if args.is_empty() {
                let _ = stdout.write(b"Usage: write <filename> <content>\r\n").await;
                return Ok(());
            }

            if !crate::fs::is_initialized() {
                let _ = stdout.write(b"Error: Filesystem not initialized\r\n").await;
                return Ok(());
            }

            let (filename, content) = split_first_word(args);
            if content.is_empty() {
                let _ = stdout.write(b"Usage: write <filename> <content>\r\n").await;
                return Ok(());
            }

            let path = core::str::from_utf8(filename).unwrap_or("");
            match async_fs::write_file(path, content).await {
                Ok(()) => {
                    let msg = format!("Wrote {} bytes to {}\r\n", content.len(), path);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
                Err(e) => {
                    let msg = format!("Error writing file: {}\r\n", e);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
            }
            Ok(())
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
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            if args.is_empty() {
                let _ = stdout
                    .write(b"Usage: append <filename> <content>\r\n")
                    .await;
                return Ok(());
            }

            if !crate::fs::is_initialized() {
                let _ = stdout.write(b"Error: Filesystem not initialized\r\n").await;
                return Ok(());
            }

            let (filename, content) = split_first_word(args);
            if content.is_empty() {
                let _ = stdout
                    .write(b"Usage: append <filename> <content>\r\n")
                    .await;
                return Ok(());
            }

            let path = core::str::from_utf8(filename).unwrap_or("");
            match async_fs::append_file(path, content).await {
                Ok(()) => {
                    let msg = format!("Appended {} bytes to {}\r\n", content.len(), path);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
                Err(e) => {
                    let msg = format!("Error appending to file: {}\r\n", e);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
            }
            Ok(())
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
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            if args.is_empty() {
                let _ = stdout.write(b"Usage: rm <filename>\r\n").await;
                return Ok(());
            }

            if !crate::fs::is_initialized() {
                let _ = stdout.write(b"Error: Filesystem not initialized\r\n").await;
                return Ok(());
            }

            let path = core::str::from_utf8(args).unwrap_or("");
            match async_fs::remove_file(path).await {
                Ok(()) => {
                    let msg = format!("Removed: {}\r\n", path);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
                Err(e) => {
                    let msg = format!("Error removing file: {}\r\n", e);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
            }
            Ok(())
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
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            if args.is_empty() {
                let _ = stdout.write(b"Usage: mkdir <dirname>\r\n").await;
                return Ok(());
            }

            if !crate::fs::is_initialized() {
                let _ = stdout.write(b"Error: Filesystem not initialized\r\n").await;
                return Ok(());
            }

            let path = core::str::from_utf8(args).unwrap_or("");
            match async_fs::create_dir(path).await {
                Ok(()) => {
                    let msg = format!("Created directory: {}\r\n", path);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
                Err(e) => {
                    let msg = format!("Error creating directory: {}\r\n", e);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
            }
            Ok(())
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
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            if !crate::fs::is_initialized() {
                let _ = stdout.write(b"Error: Filesystem not initialized\r\n").await;
                return Ok(());
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
                    let _ = stdout.write(info.as_bytes()).await;
                }
                Err(e) => {
                    let msg = format!("Error getting filesystem stats: {}\r\n", e);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
            }
            Ok(())
        })
    }
}

/// Static instance
pub static DF_CMD: DfCommand = DfCommand;

// ============================================================================
// Mv Command
// ============================================================================

/// Mv command - move/rename files
pub struct MvCommand;

impl Command for MvCommand {
    fn name(&self) -> &'static str {
        "mv"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["move", "rename"]
    }
    fn description(&self) -> &'static str {
        "Move or rename a file"
    }
    fn usage(&self) -> &'static str {
        "mv <source> <destination>"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            // Parse source and destination paths
            let (source, rest) = split_first_word(args);
            let (dest, _) = split_first_word(rest);

            if source.is_empty() || dest.is_empty() {
                let _ = stdout.write(b"Usage: mv <source> <destination>\r\n").await;
                return Ok(());
            }

            let source_path = match core::str::from_utf8(source) {
                Ok(s) => s.trim(),
                Err(_) => {
                    let _ = stdout.write(b"Error: Invalid source path\r\n").await;
                    return Ok(());
                }
            };

            let dest_path = match core::str::from_utf8(dest) {
                Ok(s) => s.trim(),
                Err(_) => {
                    let _ = stdout.write(b"Error: Invalid destination path\r\n").await;
                    return Ok(());
                }
            };

            // Check if source exists
            if !async_fs::exists(source_path).await {
                let msg = format!("Error: '{}' not found\r\n", source_path);
                let _ = stdout.write(msg.as_bytes()).await;
                return Ok(());
            }

            // Check if it's a directory (try to list it)
            if async_fs::list_dir(source_path).await.is_ok() {
                let _ = stdout.write(b"Error: Moving directories is not supported\r\n").await;
                return Ok(());
            }

            // Read source file
            let data = match async_fs::read_file(source_path).await {
                Ok(d) => d,
                Err(e) => {
                    let msg = format!("Error reading '{}': {}\r\n", source_path, e);
                    let _ = stdout.write(msg.as_bytes()).await;
                    return Ok(());
                }
            };

            // Write to destination
            if let Err(e) = async_fs::write_file(dest_path, &data).await {
                let msg = format!("Error writing '{}': {}\r\n", dest_path, e);
                let _ = stdout.write(msg.as_bytes()).await;
                return Ok(());
            }

            // Remove source file
            if let Err(e) = async_fs::remove_file(source_path).await {
                let msg = format!("Error removing source '{}': {}\r\n", source_path, e);
                let _ = stdout.write(msg.as_bytes()).await;
                return Ok(());
            }

            let msg = format!("Moved '{}' -> '{}'\r\n", source_path, dest_path);
            let _ = stdout.write(msg.as_bytes()).await;
            Ok(())
        })
    }
}

/// Static instance
pub static MV_CMD: MvCommand = MvCommand;

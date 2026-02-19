//! Filesystem Commands
//!
//! Commands for filesystem operations: ls, cat, write, append, rm, mkdir, df

use alloc::boxed::Box;
use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use embedded_io_async::Write;

use crate::async_fs;
use crate::shell::{Command, ShellContext, ShellError, VecWriter};
use crate::ssh::crypto::split_first_word;

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
        ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let path = if args.is_empty() {
                ctx.cwd().to_string()
            } else {
                let arg_str = core::str::from_utf8(args).unwrap_or("/");
                ctx.resolve_path(arg_str)
            };
            let path = path.as_str();

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
                        let _ = stdout.write(COLOR_DIR).await;
                        let _ = stdout.write(entry.name.as_bytes()).await;
                        let _ = stdout.write(b"/").await;
                        let _ = stdout.write(COLOR_RESET).await;
                        let _ = stdout.write(b"\r\n").await;
                    }

                    for entry in files {
                        let _ = stdout.write(entry.name.as_bytes()).await;
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
// Find Command
// ============================================================================

/// Find command - list files and directories recursively
pub struct FindCommand;

impl Command for FindCommand {
    fn name(&self) -> &'static str {
        "find"
    }
    fn description(&self) -> &'static str {
        "List files and directories recursively"
    }
    fn usage(&self) -> &'static str {
        "find [path]"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let path = if args.is_empty() {
                ctx.cwd().to_string()
            } else {
                let arg_str = core::str::from_utf8(args).unwrap_or(".");
                ctx.resolve_path(arg_str)
            };

            if !crate::fs::is_initialized() {
                let _ = stdout.write(b"Error: Filesystem not initialized\r\n").await;
                return Ok(());
            }

            match find_recursive(&path, stdout).await {
                Ok(_) => {}
                Err(e) => {
                    let msg = format!("Error finding files: {}\r\n", e);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
            }
            Ok(())
        })
    }
}

/// Recursively find and list files/directories
async fn find_recursive(path: &str, stdout: &mut VecWriter) -> Result<(), crate::fs::FsError> {
    // List directory contents
    let entries = match async_fs::list_dir(path).await {
        Ok(e) => e,
        Err(e) => return Err(e),
    };

    for entry in entries {
        // Yield to allow other tasks to run
        crate::threading::yield_now();

        // Construct full path
        let full_path = if path == "/" {
            format!("/{}", entry.name)
        } else if path.ends_with('/') {
            format!("{}{}", path, entry.name)
        } else {
            format!("{}/{}", path, entry.name)
        };

        // Print path
        let line = format!("{}\r\n", full_path);
        let _ = stdout.write(line.as_bytes()).await;

        // If directory, recurse
        if entry.is_dir {
            // Avoid infinite recursion for . and .. (though our FS doesn't usually return them)
            if entry.name != "." && entry.name != ".." {
                Box::pin(find_recursive(&full_path, stdout)).await?;
            }
        }
    }
    Ok(())
}

/// Static instance
pub static FIND_CMD: FindCommand = FindCommand;

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
        ctx: &'a mut ShellContext,
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

            let arg_str = core::str::from_utf8(args).unwrap_or("");
            let path = ctx.resolve_path(arg_str);
            match async_fs::read_to_string(&path).await {
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
        ctx: &'a mut ShellContext,
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

            let filename_str = core::str::from_utf8(filename).unwrap_or("");
            let path = ctx.resolve_path(filename_str);
            match async_fs::write_file(&path, content).await {
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
        ctx: &'a mut ShellContext,
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

            let filename_str = core::str::from_utf8(filename).unwrap_or("");
            let path = ctx.resolve_path(filename_str);
            match async_fs::append_file(&path, content).await {
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

/// Rm command - remove file or directory
pub struct RmCommand;

impl Command for RmCommand {
    fn name(&self) -> &'static str {
        "rm"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["del"]
    }
    fn description(&self) -> &'static str {
        "Remove file or directory"
    }
    fn usage(&self) -> &'static str {
        "rm [-r] <path>"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            if args.is_empty() {
                let _ = stdout.write(b"Usage: rm [-r] <path>\r\n").await;
                return Ok(());
            }

            if !crate::fs::is_initialized() {
                let _ = stdout.write(b"Error: Filesystem not initialized\r\n").await;
                return Ok(());
            }

            // Parse arguments for -r flag
            let arg_str = core::str::from_utf8(args).unwrap_or("");
            let mut recursive = false;
            let mut path_arg = arg_str.trim();

            // Check for -r or -rf flags
            if path_arg.starts_with("-r ") || path_arg.starts_with("-rf ") {
                recursive = true;
                path_arg = path_arg.split_once(' ').map(|(_, rest)| rest.trim()).unwrap_or("");
            } else if path_arg == "-r" || path_arg == "-rf" {
                let _ = stdout.write(b"Usage: rm [-r] <path>\r\n").await;
                return Ok(());
            }

            if path_arg.is_empty() {
                let _ = stdout.write(b"Usage: rm [-r] <path>\r\n").await;
                return Ok(());
            }

            let path = ctx.resolve_path(path_arg);

            // Check if it's a directory
            let is_dir = async_fs::list_dir(&path).await.is_ok();

            if is_dir {
                if !recursive {
                    let msg = format!("Error: '{}' is a directory, use -r to remove\r\n", path);
                    let _ = stdout.write(msg.as_bytes()).await;
                    return Ok(());
                }

                // Recursive directory removal
                match remove_dir_recursive(&path).await {
                    Ok(()) => {
                        let msg = format!("Removed: {}\r\n", path);
                        let _ = stdout.write(msg.as_bytes()).await;
                    }
                    Err(e) => {
                        let msg = format!("Error removing directory: {}\r\n", e);
                        let _ = stdout.write(msg.as_bytes()).await;
                    }
                }
            } else {
                // Regular file removal
                match async_fs::remove_file(&path).await {
                    Ok(()) => {
                        let msg = format!("Removed: {}\r\n", path);
                        let _ = stdout.write(msg.as_bytes()).await;
                    }
                    Err(e) => {
                        let msg = format!("Error removing file: {}\r\n", e);
                        let _ = stdout.write(msg.as_bytes()).await;
                    }
                }
            }
            Ok(())
        })
    }
}

/// Error type for recursive removal with path context
#[derive(Debug)]
pub struct RemoveError {
    pub path: alloc::string::String,
    pub operation: &'static str,
    pub error: crate::fs::FsError,
}

impl core::fmt::Display for RemoveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} '{}': {}", self.operation, self.path, self.error)
    }
}

/// Recursively remove a directory and all its contents
async fn remove_dir_recursive(path: &str) -> Result<(), RemoveError> {
    // List directory contents first to get all entries
    let entries = async_fs::list_dir(path).await.map_err(|e| RemoveError {
        path: alloc::string::String::from(path),
        operation: "listing",
        error: e,
    })?;

    // Remove all entries
    for entry in entries {
        // Yield between operations to allow other tasks to run
        crate::threading::yield_now();

        let entry_path = if path == "/" {
            format!("/{}", entry.name)
        } else {
            format!("{}/{}", path, entry.name)
        };

        if entry.is_dir {
            // Recursively remove subdirectory
            match Box::pin(remove_dir_recursive(&entry_path)).await {
                Ok(()) => {}
                // Skip entries that don't exist or have invalid inodes
                Err(e)
                    if e.error == crate::fs::FsError::NotFound
                        || e.error == crate::fs::FsError::IoError =>
                {
                    // Already removed or invalid, skip
                }
                Err(e) => return Err(e),
            }
        } else {
            // Try to remove as file
            match async_fs::remove_file(&entry_path).await {
                Ok(()) => {}
                // Skip entries that don't exist or have invalid inodes
                Err(crate::fs::FsError::NotFound) | Err(crate::fs::FsError::IoError) => {
                    // Already removed or invalid inode, skip
                }
                Err(crate::fs::FsError::NotAFile) => {
                    // Actually a directory, try recursive removal
                    match Box::pin(remove_dir_recursive(&entry_path)).await {
                        Ok(()) => {}
                        Err(e)
                            if e.error == crate::fs::FsError::NotFound
                                || e.error == crate::fs::FsError::IoError => {}
                        Err(e) => return Err(e),
                    }
                }
                Err(e) => {
                    return Err(RemoveError {
                        path: entry_path,
                        operation: "removing file",
                        error: e,
                    });
                }
            }
        }
    }

    // Remove the now-empty directory
    async_fs::remove_dir(path).await.map_err(|e| RemoveError {
        path: alloc::string::String::from(path),
        operation: "removing directory",
        error: e,
    })
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
        ctx: &'a mut ShellContext,
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

            let arg_str = core::str::from_utf8(args).unwrap_or("");
            let path = ctx.resolve_path(arg_str);
            match async_fs::create_dir(&path).await {
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
        _ctx: &'a mut ShellContext,
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
        ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            // Parse source and destination paths
            let (source, rest) = split_first_word(args);
            let (dest, _) = split_first_word(rest);

            if source.is_empty() || dest.is_empty() {
                let _ = stdout.write(b"Usage: mv <source> <destination>\r\n").await;
                return Ok(());
            }

            let source_str = match core::str::from_utf8(source) {
                Ok(s) => s.trim(),
                Err(_) => {
                    let _ = stdout.write(b"Error: Invalid source path\r\n").await;
                    return Ok(());
                }
            };

            let dest_str = match core::str::from_utf8(dest) {
                Ok(s) => s.trim(),
                Err(_) => {
                    let _ = stdout.write(b"Error: Invalid destination path\r\n").await;
                    return Ok(());
                }
            };

            let source_path = ctx.resolve_path(source_str);
            let dest_path = ctx.resolve_path(dest_str);

            // Check if source exists
            if !async_fs::exists(&source_path).await {
                let msg = format!("Error: '{}' not found\r\n", source_path);
                let _ = stdout.write(msg.as_bytes()).await;
                return Ok(());
            }

            // Try atomic rename first
            match async_fs::rename(&source_path, &dest_path).await {
                Ok(()) => {
                    let msg = format!("Moved '{}' -> '{}'\r\n", source_path, dest_path);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
                Err(crate::fs::FsError::NotSupported) => {
                    // Fall back to copy+delete for cross-FS or unsupported rename (files only)
                    if async_fs::list_dir(&source_path).await.is_ok() {
                        let _ = stdout.write(b"Error: Moving directories across filesystems is not supported\r\n").await;
                        return Ok(());
                    }

                    // Read source file
                    let data = match async_fs::read_file(&source_path).await {
                        Ok(d) => d,
                        Err(e) => {
                            let msg = format!("Error reading '{}': {}\r\n", source_path, e);
                            let _ = stdout.write(msg.as_bytes()).await;
                            return Ok(());
                        }
                    };

                    // Write to destination
                    if let Err(e) = async_fs::write_file(&dest_path, &data).await {
                        let msg = format!("Error writing '{}': {}\r\n", dest_path, e);
                        let _ = stdout.write(msg.as_bytes()).await;
                        return Ok(());
                    }

                    // Remove source file
                    if let Err(e) = async_fs::remove_file(&source_path).await {
                        let msg = format!("Error removing source '{}': {}\r\n", source_path, e);
                        let _ = stdout.write(msg.as_bytes()).await;
                        return Ok(());
                    }

                    let msg = format!("Moved '{}' -> '{}' (via copy)\r\n", source_path, dest_path);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
                Err(e) => {
                    let msg = format!("Error moving '{}': {}\r\n", source_path, e);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
            }
            Ok(())
        })
    }
}

/// Static instance
pub static MV_CMD: MvCommand = MvCommand;

// ============================================================================
// Cp Command
// ============================================================================

/// Cp command - copy files
pub struct CpCommand;

impl Command for CpCommand {
    fn name(&self) -> &'static str {
        "cp"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["copy"]
    }
    fn description(&self) -> &'static str {
        "Copy files or directories"
    }
    fn usage(&self) -> &'static str {
        "cp [-r] <source> <destination>"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            if args.is_empty() {
                let _ = stdout.write(b"Usage: cp [-r] <source> <destination>\r\n").await;
                return Ok(());
            }

            let arg_str = core::str::from_utf8(args).unwrap_or("");
            let mut recursive = false;
            let mut remaining_args = arg_str.trim();

            if remaining_args.starts_with("-r ") {
                recursive = true;
                remaining_args = &remaining_args[3..].trim();
            } else if remaining_args == "-r" {
                let _ = stdout.write(b"Usage: cp [-r] <source> <destination>\r\n").await;
                return Ok(());
            }

            let (source_str, dest_str) = match remaining_args.split_once(' ') {
                Some((s, d)) => (s.trim(), d.trim()),
                None => {
                    let _ = stdout.write(b"Usage: cp [-r] <source> <destination>\r\n").await;
                    return Ok(());
                }
            };

            if source_str.is_empty() || dest_str.is_empty() {
                let _ = stdout.write(b"Usage: cp [-r] <source> <destination>\r\n").await;
                return Ok(());
            }

            let source_path = ctx.resolve_path(source_str);
            let dest_path = ctx.resolve_path(dest_str);

            if !async_fs::exists(&source_path).await {
                let msg = format!("Error: '{}' not found\r\n", source_path);
                let _ = stdout.write(msg.as_bytes()).await;
                return Ok(());
            }

            // Check if source is a directory
            let is_dir = async_fs::list_dir(&source_path).await.is_ok();

            if is_dir {
                if !recursive {
                    let msg = format!("Error: '{}' is a directory (use -r to copy recursively)\r\n", source_path);
                    let _ = stdout.write(msg.as_bytes()).await;
                    return Ok(());
                }

                match copy_dir_recursive(&source_path, &dest_path).await {
                    Ok(()) => {
                        let msg = format!("Copied directory '{}' -> '{}'\r\n", source_path, dest_path);
                        let _ = stdout.write(msg.as_bytes()).await;
                    }
                    Err(e) => {
                        let msg = format!("Error copying directory: {}\r\n", e);
                        let _ = stdout.write(msg.as_bytes()).await;
                    }
                }
            } else {
                // Copy file
                match copy_file(&source_path, &dest_path).await {
                    Ok(()) => {
                        let msg = format!("Copied '{}' -> '{}'\r\n", source_path, dest_path);
                        let _ = stdout.write(msg.as_bytes()).await;
                    }
                    Err(e) => {
                        let msg = format!("Error copying file: {}\r\n", e);
                        let _ = stdout.write(msg.as_bytes()).await;
                    }
                }
            }
            Ok(())
        })
    }
}

/// Helper to copy a single file
async fn copy_file(source: &str, dest: &str) -> Result<(), crate::fs::FsError> {
    let data = async_fs::read_file(source).await?;
    
    // If dest is a directory, append filename
    let mut actual_dest = dest.to_string();
    if async_fs::list_dir(dest).await.is_ok() {
        let (_, filename) = crate::vfs::split_path(source);
        actual_dest = if dest.ends_with('/') {
            format!("{}{}", dest, filename)
        } else {
            format!("{}/{}", dest, filename)
        };
    }
    
    async_fs::write_file(&actual_dest, &data).await
}

/// Helper to copy directory recursively
async fn copy_dir_recursive(source: &str, dest: &str) -> Result<(), crate::fs::FsError> {
    // Create destination directory
    if !async_fs::exists(dest).await {
        async_fs::create_dir(dest).await?;
    }

    let entries = async_fs::list_dir(source).await?;
    for entry in entries {
        crate::threading::yield_now();

        let src_path = if source == "/" {
            format!("/{}", entry.name)
        } else {
            format!("{}/{}", source, entry.name)
        };

        let dst_path = if dest == "/" {
            format!("/{}", entry.name)
        } else {
            format!("{}/{}", dest, entry.name)
        };

        if entry.is_dir {
            Box::pin(copy_dir_recursive(&src_path, &dst_path)).await?;
        } else {
            let data = async_fs::read_file(&src_path).await?;
            async_fs::write_file(&dst_path, &data).await?;
        }
    }
    Ok(())
}

/// Static instance
pub static CP_CMD: CpCommand = CpCommand;

// ============================================================================
// Mount Command
// ============================================================================

/// Mount command - show mounted filesystems
pub struct MountCommand;

impl Command for MountCommand {
    fn name(&self) -> &'static str {
        "mount"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["mounts"]
    }
    fn description(&self) -> &'static str {
        "Show mounted filesystems"
    }

    fn execute<'a>(
        &'a self,
        _args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        _ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            if !crate::fs::is_initialized() {
                let _ = stdout.write(b"Error: Filesystem not initialized\r\n").await;
                return Ok(());
            }

            match crate::vfs::list_mounts() {
                Ok(mounts) => {
                    if mounts.is_empty() {
                        let _ = stdout.write(b"No filesystems mounted\r\n").await;
                    } else {
                        for mount in mounts {
                            let line = format!("{} on {} type {}\r\n", mount.fs_type, mount.path, mount.fs_type);
                            let _ = stdout.write(line.as_bytes()).await;
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("Error listing mounts: {}\r\n", e);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
            }
            Ok(())
        })
    }
}

/// Static instance
pub static MOUNT_CMD: MountCommand = MountCommand;

//! Shell Command Handler (Async)
//!
//! Provides async command execution for the SSH shell.
//! Commands include basic utilities and filesystem operations.

use alloc::vec::Vec;

use crate::akuma::AKUMA_79;
use crate::async_fs;
use crate::network;
use crate::ssh_crypto::{split_first_word, trim_bytes};

// ============================================================================
// Command Execution
// ============================================================================

/// Execute a shell command and return the response
pub async fn execute_command(line: &[u8]) -> Vec<u8> {
    let line = trim_bytes(line);
    if line.is_empty() {
        return Vec::new();
    }

    let (cmd, args) = split_first_word(line);
    let mut response = Vec::new();

    match cmd {
        b"echo" => cmd_echo(args, &mut response),
        b"akuma" => cmd_akuma(&mut response),
        b"quit" | b"exit" => cmd_quit(&mut response),
        b"stats" => cmd_stats(&mut response),
        b"free" | b"mem" => cmd_free(&mut response),
        b"ls" | b"dir" => cmd_ls(args, &mut response).await,
        b"cat" | b"read" => cmd_cat(args, &mut response).await,
        b"write" => cmd_write(args, &mut response).await,
        b"append" => cmd_append(args, &mut response).await,
        b"rm" | b"del" => cmd_rm(args, &mut response).await,
        b"mkdir" => cmd_mkdir(args, &mut response).await,
        b"df" | b"diskfree" => cmd_df(&mut response).await,
        b"help" => cmd_help(&mut response),
        _ => {
            response.extend_from_slice(b"Unknown command: ");
            response.extend_from_slice(cmd);
            response.extend_from_slice(b"\r\nType 'help' for available commands.\r\n");
        }
    }

    response
}

/// Check if the given line is a quit/exit command
pub fn is_quit_command(line: &[u8]) -> bool {
    let line = trim_bytes(line);
    let (cmd, _) = split_first_word(line);
    cmd == b"quit" || cmd == b"exit"
}

// ============================================================================
// Individual Commands (Sync)
// ============================================================================

fn cmd_echo(args: &[u8], response: &mut Vec<u8>) {
    if !args.is_empty() {
        response.extend_from_slice(args);
    }
    response.extend_from_slice(b"\r\n");
}

fn cmd_akuma(response: &mut Vec<u8>) {
    // Display ASCII art
    for &byte in AKUMA_79 {
        if byte == b'\n' {
            response.extend_from_slice(b"\r\n");
        } else {
            response.push(byte);
        }
    }
    if !AKUMA_79.ends_with(b"\n") {
        response.extend_from_slice(b"\r\n");
    }
}

fn cmd_quit(response: &mut Vec<u8>) {
    response.extend_from_slice(b"Goodbye!\r\n");
}

fn cmd_stats(response: &mut Vec<u8>) {
    let (connections, bytes_rx, bytes_tx) = network::get_stats();
    let stats = alloc::format!(
        "Network Statistics:\r\n  Connections: {}\r\n  Bytes RX: {}\r\n  Bytes TX: {}\r\n",
        connections,
        bytes_rx,
        bytes_tx
    );
    response.extend_from_slice(stats.as_bytes());
}

fn cmd_free(response: &mut Vec<u8>) {
    let stats = crate::allocator::stats();

    let allocated_kb = stats.allocated / 1024;
    let free_kb = stats.free / 1024;
    let peak_kb = stats.peak_allocated / 1024;
    let heap_kb = stats.heap_size / 1024;
    let heap_mb = stats.heap_size / 1024 / 1024;

    // Calculate percentages
    let used_percent = if stats.heap_size > 0 {
        (stats.allocated * 100) / stats.heap_size
    } else {
        0
    };

    let info = alloc::format!(
        "Memory Statistics:\r\n\
         \r\n\
                      total       used       free\r\n\
         Mem:    {:>8} KB {:>8} KB {:>8} KB\r\n\
         \r\n\
         Usage:       {}%\r\n\
         Peak:        {} KB\r\n\
         Allocs:      {}\r\n\
         Heap size:   {} MB\r\n",
        heap_kb,
        allocated_kb,
        free_kb,
        used_percent,
        peak_kb,
        stats.allocation_count,
        heap_mb
    );
    response.extend_from_slice(info.as_bytes());
}

fn cmd_help(response: &mut Vec<u8>) {
    response.extend_from_slice(b"Available commands:\r\n");
    response.extend_from_slice(b"  echo <text>           - Echo back text\r\n");
    response.extend_from_slice(b"  akuma                 - Display ASCII art\r\n");
    response.extend_from_slice(b"  stats                 - Show network statistics\r\n");
    response.extend_from_slice(b"  free                  - Show memory usage\r\n");
    response.extend_from_slice(b"\r\nFilesystem commands:\r\n");
    response.extend_from_slice(b"  ls [path]             - List directory contents\r\n");
    response.extend_from_slice(b"  cat <file>            - Display file contents\r\n");
    response.extend_from_slice(b"  write <file> <text>   - Write text to file\r\n");
    response.extend_from_slice(b"  append <file> <text>  - Append text to file\r\n");
    response.extend_from_slice(b"  rm <file>             - Remove file\r\n");
    response.extend_from_slice(b"  mkdir <dir>           - Create directory\r\n");
    response.extend_from_slice(b"  df                    - Show disk usage\r\n");
    response.extend_from_slice(b"\r\n  help                  - Show this help\r\n");
    response.extend_from_slice(b"  quit/exit             - Close connection\r\n");
}

// ============================================================================
// Individual Commands (Async - Filesystem)
// ============================================================================

async fn cmd_ls(args: &[u8], response: &mut Vec<u8>) {
    let path = if args.is_empty() {
        "/"
    } else {
        core::str::from_utf8(args).unwrap_or("/")
    };

    if !crate::fs::is_initialized() {
        response.extend_from_slice(b"Error: Filesystem not initialized\r\n");
        return;
    }

    match async_fs::list_dir(path).await {
        Ok(entries) => {
            if entries.is_empty() {
                // Nothing to show (empty directory)
                return;
            }

            // Collect entries: directories first, then files, both sorted alphabetically
            let mut dirs: Vec<_> = entries.iter().filter(|e| e.is_dir).collect();
            let mut files: Vec<_> = entries.iter().filter(|e| !e.is_dir).collect();

            dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

            // ANSI color codes for zsh-like output
            const COLOR_DIR: &[u8] = b"\x1b[1;34m"; // Bold blue for directories
            const COLOR_RESET: &[u8] = b"\x1b[0m";

            // Display directories first (with trailing /)
            for entry in dirs {
                let name = entry.name.to_lowercase();
                response.extend_from_slice(COLOR_DIR);
                response.extend_from_slice(name.as_bytes());
                response.extend_from_slice(b"/");
                response.extend_from_slice(COLOR_RESET);
                response.extend_from_slice(b"\r\n");
            }

            // Display files
            for entry in files {
                let name = entry.name.to_lowercase();
                response.extend_from_slice(name.as_bytes());
                response.extend_from_slice(b"\r\n");
            }
        }
        Err(e) => {
            let msg = alloc::format!("Error listing directory: {}\r\n", e);
            response.extend_from_slice(msg.as_bytes());
        }
    }
}

async fn cmd_cat(args: &[u8], response: &mut Vec<u8>) {
    if args.is_empty() {
        response.extend_from_slice(b"Usage: cat <filename>\r\n");
        return;
    }

    if !crate::fs::is_initialized() {
        response.extend_from_slice(b"Error: Filesystem not initialized\r\n");
        return;
    }

    let path = core::str::from_utf8(args).unwrap_or("");
    match async_fs::read_to_string(path).await {
        Ok(content) => {
            // Convert \n to \r\n for SSH terminal
            for line in content.split('\n') {
                response.extend_from_slice(line.as_bytes());
                response.extend_from_slice(b"\r\n");
            }
        }
        Err(e) => {
            let msg = alloc::format!("Error reading file: {}\r\n", e);
            response.extend_from_slice(msg.as_bytes());
        }
    }
}

async fn cmd_write(args: &[u8], response: &mut Vec<u8>) {
    if args.is_empty() {
        response.extend_from_slice(b"Usage: write <filename> <content>\r\n");
        return;
    }

    if !crate::fs::is_initialized() {
        response.extend_from_slice(b"Error: Filesystem not initialized\r\n");
        return;
    }

    let (filename, content) = split_first_word(args);
    if content.is_empty() {
        response.extend_from_slice(b"Usage: write <filename> <content>\r\n");
        return;
    }

    let path = core::str::from_utf8(filename).unwrap_or("");
    match async_fs::write_file(path, content).await {
        Ok(()) => {
            let msg = alloc::format!("Wrote {} bytes to {}\r\n", content.len(), path);
            response.extend_from_slice(msg.as_bytes());
        }
        Err(e) => {
            let msg = alloc::format!("Error writing file: {}\r\n", e);
            response.extend_from_slice(msg.as_bytes());
        }
    }
}

async fn cmd_append(args: &[u8], response: &mut Vec<u8>) {
    if args.is_empty() {
        response.extend_from_slice(b"Usage: append <filename> <content>\r\n");
        return;
    }

    if !crate::fs::is_initialized() {
        response.extend_from_slice(b"Error: Filesystem not initialized\r\n");
        return;
    }

    let (filename, content) = split_first_word(args);
    if content.is_empty() {
        response.extend_from_slice(b"Usage: append <filename> <content>\r\n");
        return;
    }

    let path = core::str::from_utf8(filename).unwrap_or("");
    match async_fs::append_file(path, content).await {
        Ok(()) => {
            let msg = alloc::format!("Appended {} bytes to {}\r\n", content.len(), path);
            response.extend_from_slice(msg.as_bytes());
        }
        Err(e) => {
            let msg = alloc::format!("Error appending to file: {}\r\n", e);
            response.extend_from_slice(msg.as_bytes());
        }
    }
}

async fn cmd_rm(args: &[u8], response: &mut Vec<u8>) {
    if args.is_empty() {
        response.extend_from_slice(b"Usage: rm <filename>\r\n");
        return;
    }

    if !crate::fs::is_initialized() {
        response.extend_from_slice(b"Error: Filesystem not initialized\r\n");
        return;
    }

    let path = core::str::from_utf8(args).unwrap_or("");
    match async_fs::remove_file(path).await {
        Ok(()) => {
            let msg = alloc::format!("Removed: {}\r\n", path);
            response.extend_from_slice(msg.as_bytes());
        }
        Err(e) => {
            let msg = alloc::format!("Error removing file: {}\r\n", e);
            response.extend_from_slice(msg.as_bytes());
        }
    }
}

async fn cmd_mkdir(args: &[u8], response: &mut Vec<u8>) {
    if args.is_empty() {
        response.extend_from_slice(b"Usage: mkdir <dirname>\r\n");
        return;
    }

    if !crate::fs::is_initialized() {
        response.extend_from_slice(b"Error: Filesystem not initialized\r\n");
        return;
    }

    let path = core::str::from_utf8(args).unwrap_or("");
    match async_fs::create_dir(path).await {
        Ok(()) => {
            let msg = alloc::format!("Created directory: {}\r\n", path);
            response.extend_from_slice(msg.as_bytes());
        }
        Err(e) => {
            let msg = alloc::format!("Error creating directory: {}\r\n", e);
            response.extend_from_slice(msg.as_bytes());
        }
    }
}

async fn cmd_df(response: &mut Vec<u8>) {
    if !crate::fs::is_initialized() {
        response.extend_from_slice(b"Error: Filesystem not initialized\r\n");
        return;
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
            let info = alloc::format!(
                "Filesystem Statistics:\r\n  Total:  {} KB\r\n  Used:   {} KB ({}%)\r\n  Free:   {} KB\r\n  Cluster size: {} bytes\r\n",
                total_kb,
                used_kb,
                percent_used,
                free_kb,
                stats.cluster_size
            );
            response.extend_from_slice(info.as_bytes());
        }
        Err(e) => {
            let msg = alloc::format!("Error getting filesystem stats: {}\r\n", e);
            response.extend_from_slice(msg.as_bytes());
        }
    }
}

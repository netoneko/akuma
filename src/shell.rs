//! Shell Command Handler
//!
//! Provides command execution for the SSH shell.
//! Commands include basic utilities and filesystem operations.

use alloc::vec::Vec;

use crate::akuma::AKUMA_79;
use crate::network;
use crate::ssh_crypto::{split_first_word, trim_bytes};

// ============================================================================
// Command Execution
// ============================================================================

/// Execute a shell command and return the response
pub fn execute_command(line: &[u8]) -> Vec<u8> {
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
        b"ls" | b"dir" => cmd_ls(args, &mut response),
        b"cat" | b"read" => cmd_cat(args, &mut response),
        b"write" => cmd_write(args, &mut response),
        b"append" => cmd_append(args, &mut response),
        b"rm" | b"del" => cmd_rm(args, &mut response),
        b"mkdir" => cmd_mkdir(args, &mut response),
        b"df" | b"diskfree" => cmd_df(&mut response),
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
// Individual Commands
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
        connections, bytes_rx, bytes_tx
    );
    response.extend_from_slice(stats.as_bytes());
}

fn cmd_ls(args: &[u8], response: &mut Vec<u8>) {
    let path = if args.is_empty() {
        "/"
    } else {
        core::str::from_utf8(args).unwrap_or("/")
    };

    if !crate::fs::is_initialized() {
        response.extend_from_slice(b"Error: Filesystem not initialized\r\n");
        return;
    }

    match crate::fs::list_dir(path) {
        Ok(entries) => {
            if entries.is_empty() {
                response.extend_from_slice(b"(empty directory)\r\n");
            } else {
                for entry in entries {
                    if entry.is_dir {
                        let line = alloc::format!("  [DIR]  {}\r\n", entry.name);
                        response.extend_from_slice(line.as_bytes());
                    } else {
                        let line = alloc::format!(
                            "  [FILE] {:20} {:>8} bytes\r\n",
                            entry.name, entry.size
                        );
                        response.extend_from_slice(line.as_bytes());
                    }
                }
            }
        }
        Err(e) => {
            let msg = alloc::format!("Error listing directory: {}\r\n", e);
            response.extend_from_slice(msg.as_bytes());
        }
    }
}

fn cmd_cat(args: &[u8], response: &mut Vec<u8>) {
    if args.is_empty() {
        response.extend_from_slice(b"Usage: cat <filename>\r\n");
        return;
    }

    if !crate::fs::is_initialized() {
        response.extend_from_slice(b"Error: Filesystem not initialized\r\n");
        return;
    }

    let path = core::str::from_utf8(args).unwrap_or("");
    match crate::fs::read_to_string(path) {
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

fn cmd_write(args: &[u8], response: &mut Vec<u8>) {
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
    match crate::fs::write_file(path, content) {
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

fn cmd_append(args: &[u8], response: &mut Vec<u8>) {
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
    match crate::fs::append_file(path, content) {
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

fn cmd_rm(args: &[u8], response: &mut Vec<u8>) {
    if args.is_empty() {
        response.extend_from_slice(b"Usage: rm <filename>\r\n");
        return;
    }

    if !crate::fs::is_initialized() {
        response.extend_from_slice(b"Error: Filesystem not initialized\r\n");
        return;
    }

    let path = core::str::from_utf8(args).unwrap_or("");
    match crate::fs::remove_file(path) {
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

fn cmd_mkdir(args: &[u8], response: &mut Vec<u8>) {
    if args.is_empty() {
        response.extend_from_slice(b"Usage: mkdir <dirname>\r\n");
        return;
    }

    if !crate::fs::is_initialized() {
        response.extend_from_slice(b"Error: Filesystem not initialized\r\n");
        return;
    }

    let path = core::str::from_utf8(args).unwrap_or("");
    match crate::fs::create_dir(path) {
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

fn cmd_df(response: &mut Vec<u8>) {
    if !crate::fs::is_initialized() {
        response.extend_from_slice(b"Error: Filesystem not initialized\r\n");
        return;
    }

    match crate::fs::stats() {
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
                total_kb, used_kb, percent_used, free_kb, stats.cluster_size
            );
            response.extend_from_slice(info.as_bytes());
        }
        Err(e) => {
            let msg = alloc::format!("Error getting filesystem stats: {}\r\n", e);
            response.extend_from_slice(msg.as_bytes());
        }
    }
}

fn cmd_help(response: &mut Vec<u8>) {
    response.extend_from_slice(b"Available commands:\r\n");
    response.extend_from_slice(b"  echo <text>           - Echo back text\r\n");
    response.extend_from_slice(b"  akuma                 - Display ASCII art\r\n");
    response.extend_from_slice(b"  stats                 - Show network statistics\r\n");
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


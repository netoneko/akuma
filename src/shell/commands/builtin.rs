//! Built-in Shell Commands
//!
//! Basic shell commands: echo, akuma, stats, free, help, grep

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use embedded_io_async::Write;

use crate::akuma::AKUMA_79;
use crate::network;
use crate::shell::{Command, ShellError, VecWriter};

// ============================================================================
// Echo Command
// ============================================================================

/// Echo command - echoes text back
pub struct EchoCommand;

impl Command for EchoCommand {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "Echo back text"
    }
    fn usage(&self) -> &'static str {
        "echo <text>"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            if !args.is_empty() {
                let _ = stdout.write(args).await;
            }
            let _ = stdout.write(b"\r\n").await;
            Ok(())
        })
    }
}

/// Static instance
pub static ECHO_CMD: EchoCommand = EchoCommand;

// ============================================================================
// Akuma Command
// ============================================================================

/// Akuma command - displays ASCII art
pub struct AkumaCommand;

impl Command for AkumaCommand {
    fn name(&self) -> &'static str {
        "akuma"
    }
    fn description(&self) -> &'static str {
        "Display ASCII art"
    }

    fn execute<'a>(
        &'a self,
        _args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            for &byte in AKUMA_79 {
                if byte == b'\n' {
                    let _ = stdout.write(b"\r\n").await;
                } else {
                    let _ = stdout.write(&[byte]).await;
                }
            }
            if !AKUMA_79.ends_with(b"\n") {
                let _ = stdout.write(b"\r\n").await;
            }
            Ok(())
        })
    }
}

/// Static instance
pub static AKUMA_CMD: AkumaCommand = AkumaCommand;

// ============================================================================
// Stats Command
// ============================================================================

/// Stats command - shows network statistics
pub struct StatsCommand;

impl Command for StatsCommand {
    fn name(&self) -> &'static str {
        "stats"
    }
    fn description(&self) -> &'static str {
        "Show network statistics"
    }

    fn execute<'a>(
        &'a self,
        _args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let (connections, bytes_rx, bytes_tx) = network::get_stats();
            let stats = format!(
                "Network Statistics:\r\n  Connections: {}\r\n  Bytes RX: {}\r\n  Bytes TX: {}\r\n",
                connections, bytes_rx, bytes_tx
            );
            let _ = stdout.write(stats.as_bytes()).await;
            Ok(())
        })
    }
}

/// Static instance
pub static STATS_CMD: StatsCommand = StatsCommand;

// ============================================================================
// Free Command
// ============================================================================

/// Free command - shows memory usage
pub struct FreeCommand;

impl Command for FreeCommand {
    fn name(&self) -> &'static str {
        "free"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["mem"]
    }
    fn description(&self) -> &'static str {
        "Show memory usage"
    }

    fn execute<'a>(
        &'a self,
        _args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let stats = crate::allocator::stats();

            let allocated_kb = stats.allocated / 1024;
            let free_kb = stats.free / 1024;
            let peak_kb = stats.peak_allocated / 1024;
            let heap_kb = stats.heap_size / 1024;
            let heap_mb = stats.heap_size / 1024 / 1024;

            let used_percent = if stats.heap_size > 0 {
                (stats.allocated * 100) / stats.heap_size
            } else {
                0
            };

            let info = format!(
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
            let _ = stdout.write(info.as_bytes()).await;
            Ok(())
        })
    }
}

/// Static instance
pub static FREE_CMD: FreeCommand = FreeCommand;

// ============================================================================
// Help Command
// ============================================================================

/// Help command - shows available commands
pub struct HelpCommand;

impl Command for HelpCommand {
    fn name(&self) -> &'static str {
        "help"
    }
    fn description(&self) -> &'static str {
        "Show this help"
    }

    fn execute<'a>(
        &'a self,
        _args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let _ = stdout.write(b"Available commands:\r\n").await;
            let _ = stdout
                .write(b"  echo <text>           - Echo back text\r\n")
                .await;
            let _ = stdout
                .write(b"  akuma                 - Display ASCII art\r\n")
                .await;
            let _ = stdout
                .write(b"  stats                 - Show network statistics\r\n")
                .await;
            let _ = stdout
                .write(b"  free                  - Show memory usage\r\n")
                .await;
            let _ = stdout
                .write(b"  grep [-iv] <pattern>  - Filter lines by pattern\r\n")
                .await;
            let _ = stdout.write(b"\r\nFilesystem commands:\r\n").await;
            let _ = stdout
                .write(b"  ls [path]             - List directory contents\r\n")
                .await;
            let _ = stdout
                .write(b"  cat <file>            - Display file contents\r\n")
                .await;
            let _ = stdout
                .write(b"  write <file> <text>   - Write text to file\r\n")
                .await;
            let _ = stdout
                .write(b"  append <file> <text>  - Append text to file\r\n")
                .await;
            let _ = stdout
                .write(b"  rm <file>             - Remove file\r\n")
                .await;
            let _ = stdout
                .write(b"  mkdir <dir>           - Create directory\r\n")
                .await;
            let _ = stdout
                .write(b"  df                    - Show disk usage\r\n")
                .await;
            let _ = stdout.write(b"\r\nNetwork commands:\r\n").await;
            let _ = stdout
                .write(b"  curl <url>            - HTTP GET request\r\n")
                .await;
            let _ = stdout
                .write(b"  nslookup <host>       - DNS lookup with timing\r\n")
                .await;
            let _ = stdout.write(b"\r\nPipeline and redirection:\r\n").await;
            let _ = stdout
                .write(b"  cmd1 | cmd2           - Pipe output of cmd1 to cmd2\r\n")
                .await;
            let _ = stdout
                .write(b"  cmd > file            - Redirect output to file (overwrite)\r\n")
                .await;
            let _ = stdout
                .write(b"  cmd >> file           - Redirect output to file (append)\r\n")
                .await;
            let _ = stdout
                .write(b"  ls | grep txt > out   - Combine pipes with redirection\r\n")
                .await;
            let _ = stdout
                .write(b"\r\n  help                  - Show this help\r\n")
                .await;
            let _ = stdout
                .write(b"  quit/exit             - Close connection\r\n")
                .await;
            Ok(())
        })
    }
}

/// Static instance
pub static HELP_CMD: HelpCommand = HelpCommand;

// ============================================================================
// Grep Command
// ============================================================================

/// Grep command - filters lines by pattern
pub struct GrepCommand;

impl Command for GrepCommand {
    fn name(&self) -> &'static str {
        "grep"
    }
    fn description(&self) -> &'static str {
        "Filter lines by pattern"
    }
    fn usage(&self) -> &'static str {
        "grep [-i] [-v] <pattern> [file]"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            // Parse flags and pattern
            let mut case_insensitive = false;
            let mut invert_match = false;
            let mut pattern: Option<&[u8]> = None;
            let mut file_path: Option<&str> = None;

            // Simple argument parsing
            let args_str = core::str::from_utf8(args).unwrap_or("");
            let parts: Vec<&str> = args_str.split_whitespace().collect();

            let mut i = 0;
            while i < parts.len() {
                let part = parts[i];
                if part.starts_with('-') {
                    // Parse flags
                    for c in part[1..].chars() {
                        match c {
                            'i' => case_insensitive = true,
                            'v' => invert_match = true,
                            _ => {}
                        }
                    }
                } else if pattern.is_none() {
                    pattern = Some(part.as_bytes());
                } else {
                    file_path = Some(part);
                }
                i += 1;
            }

            let pattern = match pattern {
                Some(p) => p,
                None => {
                    let _ = stdout
                        .write(b"Usage: grep [-i] [-v] <pattern> [file]\r\n")
                        .await;
                    return Ok(());
                }
            };

            // Get input data - either from file or stdin
            let input_data: Vec<u8> = if let Some(path) = file_path {
                // Read from file
                if !crate::fs::is_initialized() {
                    let _ = stdout.write(b"Error: Filesystem not initialized\r\n").await;
                    return Ok(());
                }
                match crate::async_fs::read_file(path).await {
                    Ok(data) => data,
                    Err(e) => {
                        let msg = format!("Error reading file: {}\r\n", e);
                        let _ = stdout.write(msg.as_bytes()).await;
                        return Ok(());
                    }
                }
            } else if let Some(data) = stdin {
                data.to_vec()
            } else {
                let _ = stdout
                    .write(b"grep: no input (use with pipe or specify file)\r\n")
                    .await;
                return Ok(());
            };

            // Convert pattern to string for matching
            let pattern_str = core::str::from_utf8(pattern).unwrap_or("");
            let pattern_lower = if case_insensitive {
                pattern_str.to_lowercase()
            } else {
                String::new()
            };

            // Process input line by line
            let input_str = core::str::from_utf8(&input_data).unwrap_or("");
            for line in input_str.lines() {
                let matches = if case_insensitive {
                    line.to_lowercase().contains(&pattern_lower)
                } else {
                    line.contains(pattern_str)
                };

                // Apply invert flag
                let should_print = if invert_match { !matches } else { matches };

                if should_print {
                    let _ = stdout.write(line.as_bytes()).await;
                    let _ = stdout.write(b"\r\n").await;
                }
            }

            Ok(())
        })
    }
}

/// Static instance
pub static GREP_CMD: GrepCommand = GrepCommand;

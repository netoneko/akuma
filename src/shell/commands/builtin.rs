//! Built-in Shell Commands
//!
//! Basic shell commands: echo, akuma, stats, free, help

use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use crate::akuma::AKUMA_79;
use crate::network;
use crate::shell::{Command, ShellError};

// ============================================================================
// Echo Command
// ============================================================================

/// Echo command - echoes text back
pub struct EchoCommand;

impl Command for EchoCommand {
    fn name(&self) -> &'static str { "echo" }
    fn description(&self) -> &'static str { "Echo back text" }
    fn usage(&self) -> &'static str { "echo <text>" }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ShellError>> + 'a>> {
        Box::pin(async move {
            let mut response = Vec::new();
            if !args.is_empty() {
                response.extend_from_slice(args);
            }
            response.extend_from_slice(b"\r\n");
            Ok(response)
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
    fn name(&self) -> &'static str { "akuma" }
    fn description(&self) -> &'static str { "Display ASCII art" }

    fn execute<'a>(
        &'a self,
        _args: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ShellError>> + 'a>> {
        Box::pin(async move {
            let mut response = Vec::new();
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
            Ok(response)
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
    fn name(&self) -> &'static str { "stats" }
    fn description(&self) -> &'static str { "Show network statistics" }

    fn execute<'a>(
        &'a self,
        _args: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ShellError>> + 'a>> {
        Box::pin(async move {
            let (connections, bytes_rx, bytes_tx) = network::get_stats();
            let stats = format!(
                "Network Statistics:\r\n  Connections: {}\r\n  Bytes RX: {}\r\n  Bytes TX: {}\r\n",
                connections, bytes_rx, bytes_tx
            );
            Ok(stats.into_bytes())
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
    fn name(&self) -> &'static str { "free" }
    fn aliases(&self) -> &'static [&'static str] { &["mem"] }
    fn description(&self) -> &'static str { "Show memory usage" }

    fn execute<'a>(
        &'a self,
        _args: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ShellError>> + 'a>> {
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
                heap_kb, allocated_kb, free_kb,
                used_percent, peak_kb, stats.allocation_count, heap_mb
            );
            Ok(info.into_bytes())
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
    fn name(&self) -> &'static str { "help" }
    fn description(&self) -> &'static str { "Show this help" }

    fn execute<'a>(
        &'a self,
        _args: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ShellError>> + 'a>> {
        Box::pin(async move {
            let mut response = Vec::new();
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
            response.extend_from_slice(b"\r\nNetwork commands:\r\n");
            response.extend_from_slice(b"  curl <url>            - HTTP GET request\r\n");
            response.extend_from_slice(b"\r\n  help                  - Show this help\r\n");
            response.extend_from_slice(b"  quit/exit             - Close connection\r\n");
            Ok(response)
        })
    }
}

/// Static instance
pub static HELP_CMD: HelpCommand = HelpCommand;


//! Built-in Shell Commands
//!
//! Basic shell commands: echo, akuma, stats, free, help, grep, pwd, cd

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use embedded_io_async::Write;

use crate::akuma::AKUMA_79;
use crate::network;
use crate::shell::{Command, ShellContext, ShellError, VecWriter};

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
        _ctx: &'a mut ShellContext,
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
        _ctx: &'a mut ShellContext,
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
        _ctx: &'a mut ShellContext,
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
        _ctx: &'a mut ShellContext,
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
        _ctx: &'a mut ShellContext,
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
            let _ = stdout
                .write(b"  ps                    - List running processes\r\n")
                .await;
            let _ = stdout
                .write(b"  kthreads              - List kernel threads with stack info\r\n")
                .await;
            let _ = stdout.write(b"\r\nNavigation commands:\r\n").await;
            let _ = stdout
                .write(b"  pwd                   - Print current working directory\r\n")
                .await;
            let _ = stdout
                .write(b"  cd [path]             - Change current working directory\r\n")
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
            let _ = stdout.write(b"\r\nScripting commands:\r\n").await;
            let _ = stdout
                .write(b"  rhai <file>           - Execute a Rhai script\r\n")
                .await;
            let _ = stdout
                .write(b"  Note: String interpolation (`Hello, ${x}`) not supported in no_std.\r\n")
                .await;
            let _ = stdout
                .write(b"        Use concatenation instead: \"Hello, \" + x\r\n")
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
        _ctx: &'a mut ShellContext,
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

// ============================================================================
// Ps Command
// ============================================================================

/// Ps command - list running processes
pub struct PsCommand;

impl Command for PsCommand {
    fn name(&self) -> &'static str {
        "ps"
    }
    fn description(&self) -> &'static str {
        "List running processes"
    }
    fn usage(&self) -> &'static str {
        "ps"
    }

    fn execute<'a>(
        &'a self,
        _args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        _ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            use crate::process;

            // Header
            let _ = stdout.write(b"  PID  PPID  STATE     NAME\r\n").await;

            let procs = process::list_processes();

            if procs.is_empty() {
                let _ = stdout.write(b"(no processes running)\r\n").await;
            } else {
                for p in procs {
                    let line = format!(
                        "{:>5}  {:>4}  {:<8}  {}\r\n",
                        p.pid, p.ppid, p.state, p.name
                    );
                    let _ = stdout.write(line.as_bytes()).await;
                }
            }

            Ok(())
        })
    }
}

/// Static instance
pub static PS_CMD: PsCommand = PsCommand;

// ============================================================================
// Kthreads Command
// ============================================================================

/// Kthreads command - list kernel threads with stack info
pub struct KthreadsCommand;

impl Command for KthreadsCommand {
    fn name(&self) -> &'static str {
        "kthreads"
    }
    fn description(&self) -> &'static str {
        "List kernel threads with stack info"
    }
    fn usage(&self) -> &'static str {
        "kthreads"
    }

    fn execute<'a>(
        &'a self,
        _args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        _ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            use crate::threading;

            // Header
            let _ = stdout.write(b"  TID  STATE     STACK_BASE  STACK_SIZE  STACK_USED  CANARY  TYPE         NAME\r\n").await;

            let threads = threading::list_kernel_threads();

            if threads.is_empty() {
                let _ = stdout.write(b"(no kernel threads)\r\n").await;
            } else {
                for t in threads {
                    // Format stack size and usage in KB
                    let size_kb = t.stack_size / 1024;
                    let used_kb = t.stack_used / 1024;
                    let used_pct = if t.stack_size > 0 {
                        (t.stack_used * 100) / t.stack_size
                    } else {
                        0
                    };

                    let canary_str = if t.canary_ok { "OK" } else { "FAIL" };
                    let type_str = if t.cooperative {
                        "cooperative"
                    } else {
                        "preemptive"
                    };

                    let line = format!(
                        "{:>4}  {:<8}  0x{:08x}  {:>6} KB   {:>4} KB {:>2}%  {:<6}  {:<11}  {}\r\n",
                        t.tid,
                        t.state,
                        t.stack_base,
                        size_kb,
                        used_kb,
                        used_pct,
                        canary_str,
                        type_str,
                        t.name
                    );
                    let _ = stdout.write(line.as_bytes()).await;
                }
            }

            // Summary
            let (ready, running, terminated) = threading::thread_stats();
            let total = ready + running + terminated;
            let summary = format!(
                "\r\nTotal: {} threads (ready: {}, running: {}, terminated: {})\r\n",
                total, ready, running, terminated
            );
            let _ = stdout.write(summary.as_bytes()).await;

            Ok(())
        })
    }
}

/// Static instance
pub static KTHREADS_CMD: KthreadsCommand = KthreadsCommand;

// ============================================================================
// Pwd Command
// ============================================================================

/// Pwd command - print current working directory
pub struct PwdCommand;

impl Command for PwdCommand {
    fn name(&self) -> &'static str {
        "pwd"
    }
    fn description(&self) -> &'static str {
        "Print current working directory"
    }
    fn usage(&self) -> &'static str {
        "pwd"
    }

    fn execute<'a>(
        &'a self,
        _args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let _ = stdout.write(ctx.cwd().as_bytes()).await;
            let _ = stdout.write(b"\r\n").await;
            Ok(())
        })
    }
}

/// Static instance
pub static PWD_CMD: PwdCommand = PwdCommand;

// ============================================================================
// Cd Command
// ============================================================================

/// Cd command - change current working directory
pub struct CdCommand;

impl Command for CdCommand {
    fn name(&self) -> &'static str {
        "cd"
    }
    fn description(&self) -> &'static str {
        "Change current working directory"
    }
    fn usage(&self) -> &'static str {
        "cd [path]"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let args_str = core::str::from_utf8(args).unwrap_or("").trim();

            // Default to root if no argument
            let target = if args_str.is_empty() {
                String::from("/")
            } else {
                ctx.resolve_path(args_str)
            };

            // Check if the directory exists
            if !crate::fs::is_initialized() {
                let _ = stdout.write(b"Error: Filesystem not initialized\r\n").await;
                return Ok(());
            }

            // Try to list the directory to verify it exists and is a directory
            match crate::async_fs::list_dir(&target).await {
                Ok(_) => {
                    ctx.set_cwd(&target);
                    // cd is silent on success (like in real shells)
                }
                Err(_) => {
                    let msg = format!("cd: {}: No such directory\r\n", target);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
            }

            Ok(())
        })
    }
}

/// Static instance
pub static CD_CMD: CdCommand = CdCommand;

// ============================================================================
// Uptime Command
// ============================================================================

/// Uptime command - display system uptime
pub struct UptimeCommand;

impl Command for UptimeCommand {
    fn name(&self) -> &'static str {
        "uptime"
    }
    fn description(&self) -> &'static str {
        "Display system uptime"
    }

    fn execute<'a>(
        &'a self,
        _args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        _ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let uptime_us = crate::timer::uptime_us();
            let uptime_sec = uptime_us / 1_000_000;
            let hours = uptime_sec / 3600;
            let mins = (uptime_sec % 3600) / 60;
            let secs = uptime_sec % 60;

            let msg = format!("up {}:{:02}:{:02}\r\n", hours, mins, secs);
            let _ = stdout.write(msg.as_bytes()).await;
            Ok(())
        })
    }
}

/// Static instance
pub static UPTIME_CMD: UptimeCommand = UptimeCommand;

// ============================================================================
// Pmm Command
// ============================================================================

/// Pmm command - show physical memory manager stats and debug info
pub struct PmmCommand;

impl Command for PmmCommand {
    fn name(&self) -> &'static str {
        "pmm"
    }
    fn description(&self) -> &'static str {
        "Show physical memory manager stats"
    }
    fn usage(&self) -> &'static str {
        "pmm [stats|leaks]"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        _ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            use crate::pmm;

            let args_str = core::str::from_utf8(args).unwrap_or("").trim();

            match args_str {
                "leaks" => {
                    // Show leak info (only meaningful if DEBUG_FRAME_TRACKING is enabled)
                    if pmm::DEBUG_FRAME_TRACKING {
                        let count = pmm::leak_count();
                        if count == 0 {
                            let _ = stdout.write(b"No tracked frame leaks detected.\r\n").await;
                        } else {
                            let msg = format!("Potentially leaked frames: {}\r\n", count);
                            let _ = stdout.write(msg.as_bytes()).await;
                        }

                        // Show breakdown by source
                        if let Some(stats) = pmm::tracking_stats() {
                            let breakdown = format!(
                                "Current allocations:\r\n\
                                 \r\n\
                                 Kernel:          {:>6}\r\n\
                                 User Page Table: {:>6}\r\n\
                                 User Data:       {:>6}\r\n\
                                 ELF Loader:      {:>6}\r\n\
                                 Unknown:         {:>6}\r\n\
                                 \r\n\
                                 Currently held:  {:>6}\r\n\
                                 \r\n\
                                 Cumulative (since boot):\r\n\
                                   Allocated:     {:>6}\r\n\
                                   Freed:         {:>6}\r\n",
                                stats.kernel_count,
                                stats.user_page_table_count,
                                stats.user_data_count,
                                stats.elf_loader_count,
                                stats.unknown_count,
                                stats.current_tracked,
                                stats.total_tracked,
                                stats.total_untracked
                            );
                            let _ = stdout.write(breakdown.as_bytes()).await;
                        }
                    } else {
                        let _ = stdout.write(b"DEBUG_FRAME_TRACKING is disabled.\r\n").await;
                        let _ = stdout
                            .write(b"Enable it in src/pmm.rs to track frame allocations.\r\n")
                            .await;
                    }
                }
                _ => {
                    // Default: show basic PMM stats
                    let (total, allocated, free) = pmm::stats();
                    let total_mb = (total * 4) / 1024; // 4KB pages to MB
                    let allocated_mb = (allocated * 4) / 1024;
                    let free_mb = (free * 4) / 1024;

                    let stats_msg = format!(
                        "Physical Memory Manager:\r\n\
                         \r\n\
                                     pages       MB\r\n\
                         Total:      {:>5}      {:>3}\r\n\
                         Allocated:  {:>5}      {:>3}\r\n\
                         Free:       {:>5}      {:>3}\r\n",
                        total, total_mb, allocated, allocated_mb, free, free_mb
                    );
                    let _ = stdout.write(stats_msg.as_bytes()).await;

                    // Show tracking status
                    if pmm::DEBUG_FRAME_TRACKING {
                        let _ = stdout.write(b"\r\nFrame tracking: ENABLED\r\n").await;
                        let _ = stdout
                            .write(b"Use 'pmm leaks' to see allocation breakdown.\r\n")
                            .await;
                    } else {
                        let _ = stdout.write(b"\r\nFrame tracking: DISABLED\r\n").await;
                    }
                }
            }

            Ok(())
        })
    }
}

/// Static instance
pub static PMM_CMD: PmmCommand = PmmCommand;

// ============================================================================
// Kill Command
// ============================================================================

/// Kill command - terminate a process by PID
pub struct KillCommand;

impl Command for KillCommand {
    fn name(&self) -> &'static str {
        "kill"
    }
    fn description(&self) -> &'static str {
        "Terminate a process by PID"
    }
    fn usage(&self) -> &'static str {
        "kill <pid>"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        _ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            use crate::process;

            // Parse the PID argument
            let args_str = match core::str::from_utf8(args) {
                Ok(s) => s.trim(),
                Err(_) => {
                    let _ = stdout.write(b"Error: Invalid UTF-8 in arguments\r\n").await;
                    return Err(ShellError::ExecutionFailed("invalid UTF-8"));
                }
            };

            if args_str.is_empty() {
                let _ = stdout.write(b"Usage: kill <pid>\r\n").await;
                return Err(ShellError::ExecutionFailed("missing PID argument"));
            }

            let pid: u32 = match args_str.parse() {
                Ok(p) => p,
                Err(_) => {
                    let msg = format!("Error: Invalid PID: {}\r\n", args_str);
                    let _ = stdout.write(msg.as_bytes()).await;
                    return Err(ShellError::ExecutionFailed("invalid PID"));
                }
            };

            // Try to kill the process
            match process::kill_process(pid) {
                Ok(()) => {
                    let msg = format!("Killed process {}\r\n", pid);
                    let _ = stdout.write(msg.as_bytes()).await;
                    Ok(())
                }
                Err(e) => {
                    let msg = format!("Failed to kill process {}: {}\r\n", pid, e);
                    let _ = stdout.write(msg.as_bytes()).await;
                    Err(ShellError::ExecutionFailed("kill failed"))
                }
            }
        })
    }
}

/// Static instance
pub static KILL_CMD: KillCommand = KillCommand;

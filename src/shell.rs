//! Shell Command Handler (Async)
//!
//! Provides async command execution for the SSH shell with an extensible
//! command system. Commands implement the `Command` trait and are registered
//! in a `CommandRegistry`. The `ShellSession` handles terminal I/O using
//! nostd-interactive-terminal for line editing and history.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use embedded_io_async::{Read, Write};
use nostd_interactive_terminal::terminal::{TerminalConfig, TerminalReader, ReadLineError};
use nostd_interactive_terminal::history::{History, HistoryConfig};
use nostd_interactive_terminal::writer::TerminalWriter;

use crate::akuma::AKUMA_79;
use crate::async_fs;
use crate::async_net;
use crate::network;
use crate::ssh_crypto::{split_first_word, trim_bytes};

// ============================================================================
// Shell Error Types
// ============================================================================

/// Errors that can occur during shell operations
#[derive(Debug, Clone)]
pub enum ShellError {
    /// I/O error during read/write
    IoError,
    /// Command not found
    CommandNotFound,
    /// Command execution failed
    ExecutionFailed(&'static str),
    /// Session should terminate
    Exit,
    /// End of file (Ctrl+D)
    EndOfFile,
}

// ============================================================================
// Command Trait and Registry
// ============================================================================

/// A command that can be executed by the shell
/// 
/// Commands are stateless and should be implemented as unit structs.
/// The execute method takes arguments and returns output as a Vec<u8>.
pub trait Command: Sync {
    /// The primary name of the command
    fn name(&self) -> &'static str;

    /// Alternative names for the command (aliases)
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }

    /// One-line description for help text
    fn description(&self) -> &'static str;

    /// Detailed usage information
    fn usage(&self) -> &'static str {
        ""
    }

    /// Execute the command and return the output
    /// 
    /// The args parameter contains the raw argument bytes.
    /// Returns the output to be displayed.
    fn execute<'a>(
        &'a self,
        args: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ShellError>> + 'a>>;
}

/// Maximum number of commands that can be registered
const MAX_COMMANDS: usize = 32;

/// Registry of available commands
pub struct CommandRegistry {
    commands: Vec<&'static dyn Command>,
}

impl CommandRegistry {
    /// Create a new empty registry
    pub const fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    /// Register a command
    pub fn register(&mut self, command: &'static dyn Command) {
        if self.commands.len() < MAX_COMMANDS {
            self.commands.push(command);
        }
    }

    /// Find a command by name or alias
    pub fn find(&self, name: &[u8]) -> Option<&'static dyn Command> {
        let name_str = core::str::from_utf8(name).ok()?;
        for cmd in &self.commands {
            if cmd.name() == name_str {
                return Some(*cmd);
            }
            for alias in cmd.aliases() {
                if *alias == name_str {
                    return Some(*cmd);
                }
            }
        }
        None
    }

    /// Get all registered commands
    pub fn commands(&self) -> &[&'static dyn Command] {
        &self.commands
    }
}

// ============================================================================
// Shell Session
// ============================================================================

/// Buffer size for terminal input
const TERMINAL_BUF_SIZE: usize = 256;

/// Shell session that handles terminal I/O and command execution
pub struct ShellSession<'a, R: Read, W: Write> {
    reader: TerminalReader<TERMINAL_BUF_SIZE>,
    writer_inner: &'a mut W,
    reader_inner: &'a mut R,
    registry: &'a CommandRegistry,
}

impl<'a, R: Read, W: Write> ShellSession<'a, R, W> {
    /// Create a new shell session
    pub fn new(
        reader: &'a mut R,
        writer: &'a mut W,
        registry: &'a CommandRegistry,
    ) -> Self {
        let config = TerminalConfig {
            buffer_size: TERMINAL_BUF_SIZE,
            prompt: "akuma> ",
            echo: true,
            ansi_enabled: true,
        };

        let history_config = HistoryConfig {
            max_entries: 10,
            deduplicate: true,
        };

        let terminal_reader = TerminalReader::new(config, Some(History::new(history_config)));

        Self {
            reader: terminal_reader,
            writer_inner: writer,
            reader_inner: reader,
            registry,
        }
    }

    /// Run the shell session until exit or error
    pub async fn run(&mut self) -> Result<(), ShellError> {
        // Display welcome message
        {
            let mut writer = TerminalWriter::new(self.writer_inner, true);
            let _ = writer.writeln("Welcome to Akuma Shell").await;
            let _ = writer.writeln("Type 'help' for available commands.").await;
            let _ = writer.write_str("\r\n").await;
        }

        loop {
            // Read a line using the terminal reader
            let mut writer = TerminalWriter::new(self.writer_inner, true);
            
            let line = match self.reader.read_line::<_, _, embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex>(
                self.reader_inner,
                &mut writer,
                None,
            ).await {
                Ok(line) => line,
                Err(ReadLineError::EndOfFile) => {
                    let _ = writer.writeln("\r\nGoodbye!").await;
                    return Ok(());
                }
                Err(_) => continue,
            };

            let line_bytes = line.as_bytes();
            let trimmed = trim_bytes(line_bytes);
            
            if trimmed.is_empty() {
                continue;
            }

            // Parse command and arguments
            let (cmd_name, args) = split_first_word(trimmed);

            // Check for exit/quit
            if cmd_name == b"exit" || cmd_name == b"quit" {
                let _ = writer.writeln("Goodbye!").await;
                return Ok(());
            }

            // Find and execute the command
            if let Some(cmd) = self.registry.find(cmd_name) {
                match cmd.execute(args).await {
                    Ok(output) => {
                        if !output.is_empty() {
                            if let Ok(s) = core::str::from_utf8(&output) {
                                let _ = writer.write_str(s).await;
                            }
                        }
                    }
                    Err(ShellError::Exit) => return Ok(()),
                    Err(ShellError::ExecutionFailed(msg)) => {
                        let error_msg = format!("Error: {}\r\n", msg);
                        let _ = writer.write_str(&error_msg).await;
                    }
                    Err(_) => {}
                }
            } else {
                let msg = format!(
                    "Unknown command: {}\r\nType 'help' for available commands.\r\n",
                    core::str::from_utf8(cmd_name).unwrap_or("?")
                );
                let _ = writer.write_str(&msg).await;
            }
        }
    }
}

// ============================================================================
// Built-in Commands
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

// ============================================================================
// Filesystem Commands
// ============================================================================

/// Ls command - list directory contents
pub struct LsCommand;

impl Command for LsCommand {
    fn name(&self) -> &'static str { "ls" }
    fn aliases(&self) -> &'static [&'static str] { &["dir"] }
    fn description(&self) -> &'static str { "List directory contents" }
    fn usage(&self) -> &'static str { "ls [path]" }

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

/// Cat command - display file contents
pub struct CatCommand;

impl Command for CatCommand {
    fn name(&self) -> &'static str { "cat" }
    fn aliases(&self) -> &'static [&'static str] { &["read"] }
    fn description(&self) -> &'static str { "Display file contents" }
    fn usage(&self) -> &'static str { "cat <filename>" }

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

/// Write command - write text to file
pub struct WriteCommand;

impl Command for WriteCommand {
    fn name(&self) -> &'static str { "write" }
    fn description(&self) -> &'static str { "Write text to file" }
    fn usage(&self) -> &'static str { "write <filename> <content>" }

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

/// Append command - append text to file
pub struct AppendCommand;

impl Command for AppendCommand {
    fn name(&self) -> &'static str { "append" }
    fn description(&self) -> &'static str { "Append text to file" }
    fn usage(&self) -> &'static str { "append <filename> <content>" }

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

/// Rm command - remove file
pub struct RmCommand;

impl Command for RmCommand {
    fn name(&self) -> &'static str { "rm" }
    fn aliases(&self) -> &'static [&'static str] { &["del"] }
    fn description(&self) -> &'static str { "Remove file" }
    fn usage(&self) -> &'static str { "rm <filename>" }

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

/// Mkdir command - create directory
pub struct MkdirCommand;

impl Command for MkdirCommand {
    fn name(&self) -> &'static str { "mkdir" }
    fn description(&self) -> &'static str { "Create directory" }
    fn usage(&self) -> &'static str { "mkdir <dirname>" }

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

/// Df command - show disk usage
pub struct DfCommand;

impl Command for DfCommand {
    fn name(&self) -> &'static str { "df" }
    fn aliases(&self) -> &'static [&'static str] { &["diskfree"] }
    fn description(&self) -> &'static str { "Show disk usage" }

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

// ============================================================================
// Network Commands
// ============================================================================

/// Curl command - HTTP GET request
pub struct CurlCommand;

impl Command for CurlCommand {
    fn name(&self) -> &'static str { "curl" }
    fn description(&self) -> &'static str { "HTTP GET request" }
    fn usage(&self) -> &'static str { "curl <url>" }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ShellError>> + 'a>> {
        Box::pin(async move {
            let mut response = Vec::new();
            
            if args.is_empty() {
                response.extend_from_slice(b"Usage: curl <url>\r\n");
                response.extend_from_slice(b"Example: curl http://10.0.2.2:8080/\r\n");
                return Ok(response);
            }

            let url = match core::str::from_utf8(args) {
                Ok(s) => s.trim(),
                Err(_) => {
                    response.extend_from_slice(b"Error: Invalid URL\r\n");
                    return Ok(response);
                }
            };

            match http_get(url).await {
                Ok(body) => {
                    for line in body.split('\n') {
                        response.extend_from_slice(line.as_bytes());
                        response.extend_from_slice(b"\r\n");
                    }
                }
                Err(e) => {
                    let msg = format!("Error: {}\r\n", e);
                    response.extend_from_slice(msg.as_bytes());
                }
            }
            Ok(response)
        })
    }
}

/// Perform an HTTP GET request
async fn http_get(url: &str) -> Result<String, &'static str> {
    use embassy_net::tcp::TcpSocket;
    use embassy_net::{IpAddress, IpEndpoint};
    use embassy_time::Duration;
    use embedded_io_async::Write as AsyncWrite;

    let stack = async_net::get_global_stack().ok_or("Network not initialized")?;

    let url = url.strip_prefix("http://").ok_or("Only http:// URLs supported")?;

    let (host_port, path) = url.split_once('/').unwrap_or((url, ""));
    let path = format!("/{}", path);

    let (host, port) = if let Some((h, p)) = host_port.split_once(':') {
        (h, p.parse::<u16>().map_err(|_| "Invalid port")?)
    } else {
        (host_port, 80u16)
    };

    let ip: IpAddress = if let Ok(ip) = host.parse::<embassy_net::Ipv4Address>() {
        IpAddress::Ipv4(ip)
    } else {
        match stack.dns_query(host, embassy_net::dns::DnsQueryType::A).await {
            Ok(addrs) if !addrs.is_empty() => addrs[0],
            _ => return Err("DNS lookup failed"),
        }
    };

    let mut rx_buf = [0u8; 2048];
    let mut tx_buf = [0u8; 1024];
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
    socket.set_timeout(Some(Duration::from_secs(10)));

    let endpoint = IpEndpoint::new(ip, port);
    socket.connect(endpoint).await.map_err(|_| "Connection failed")?;

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: akuma-curl/1.0\r\n\r\n",
        path, host
    );
    socket.write_all(request.as_bytes()).await.map_err(|_| "Write failed")?;

    let mut response_data = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        match socket.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => response_data.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }

    socket.close();

    let response_str = String::from_utf8(response_data).map_err(|_| "Invalid UTF-8 response")?;

    if let Some(body_start) = response_str.find("\r\n\r\n") {
        Ok(response_str[body_start + 4..].to_string())
    } else {
        Ok(response_str)
    }
}

// ============================================================================
// Static Command Instances
// ============================================================================

/// Static instances of all built-in commands
pub static ECHO_CMD: EchoCommand = EchoCommand;
pub static AKUMA_CMD: AkumaCommand = AkumaCommand;
pub static STATS_CMD: StatsCommand = StatsCommand;
pub static FREE_CMD: FreeCommand = FreeCommand;
pub static HELP_CMD: HelpCommand = HelpCommand;
pub static LS_CMD: LsCommand = LsCommand;
pub static CAT_CMD: CatCommand = CatCommand;
pub static WRITE_CMD: WriteCommand = WriteCommand;
pub static APPEND_CMD: AppendCommand = AppendCommand;
pub static RM_CMD: RmCommand = RmCommand;
pub static MKDIR_CMD: MkdirCommand = MkdirCommand;
pub static DF_CMD: DfCommand = DfCommand;
pub static CURL_CMD: CurlCommand = CurlCommand;

/// Create and populate the default command registry
pub fn create_default_registry() -> CommandRegistry {
    let mut registry = CommandRegistry::new();
    
    // Built-in commands
    registry.register(&ECHO_CMD);
    registry.register(&AKUMA_CMD);
    registry.register(&STATS_CMD);
    registry.register(&FREE_CMD);
    registry.register(&HELP_CMD);
    
    // Filesystem commands
    registry.register(&LS_CMD);
    registry.register(&CAT_CMD);
    registry.register(&WRITE_CMD);
    registry.register(&APPEND_CMD);
    registry.register(&RM_CMD);
    registry.register(&MKDIR_CMD);
    registry.register(&DF_CMD);
    
    // Network commands
    registry.register(&CURL_CMD);
    
    registry
}

// ============================================================================
// Legacy API (for backward compatibility with ssh.rs)
// ============================================================================

/// Execute a shell command and return the response (legacy API)
/// 
/// This function maintains backward compatibility with the existing ssh.rs
/// implementation. New code should use ShellSession instead.
pub async fn execute_command(line: &[u8]) -> Vec<u8> {
    let line = trim_bytes(line);
    if line.is_empty() {
        return Vec::new();
    }

    let (cmd, args) = split_first_word(line);
    let mut response = Vec::new();

    match cmd {
        b"echo" => legacy_cmd_echo(args, &mut response),
        b"akuma" => legacy_cmd_akuma(&mut response),
        b"quit" | b"exit" => legacy_cmd_quit(&mut response),
        b"stats" => legacy_cmd_stats(&mut response),
        b"free" | b"mem" => legacy_cmd_free(&mut response),
        b"ls" | b"dir" => legacy_cmd_ls(args, &mut response).await,
        b"cat" | b"read" => legacy_cmd_cat(args, &mut response).await,
        b"write" => legacy_cmd_write(args, &mut response).await,
        b"append" => legacy_cmd_append(args, &mut response).await,
        b"rm" | b"del" => legacy_cmd_rm(args, &mut response).await,
        b"mkdir" => legacy_cmd_mkdir(args, &mut response).await,
        b"df" | b"diskfree" => legacy_cmd_df(&mut response).await,
        b"curl" => legacy_cmd_curl(args, &mut response).await,
        b"help" => legacy_cmd_help(&mut response),
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

// Legacy command implementations (kept for backward compatibility)

fn legacy_cmd_echo(args: &[u8], response: &mut Vec<u8>) {
    if !args.is_empty() {
        response.extend_from_slice(args);
    }
    response.extend_from_slice(b"\r\n");
}

fn legacy_cmd_akuma(response: &mut Vec<u8>) {
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

fn legacy_cmd_quit(response: &mut Vec<u8>) {
    response.extend_from_slice(b"Goodbye!\r\n");
}

fn legacy_cmd_stats(response: &mut Vec<u8>) {
    let (connections, bytes_rx, bytes_tx) = network::get_stats();
    let stats = format!(
        "Network Statistics:\r\n  Connections: {}\r\n  Bytes RX: {}\r\n  Bytes TX: {}\r\n",
        connections, bytes_rx, bytes_tx
    );
    response.extend_from_slice(stats.as_bytes());
}

fn legacy_cmd_free(response: &mut Vec<u8>) {
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
    response.extend_from_slice(info.as_bytes());
}

fn legacy_cmd_help(response: &mut Vec<u8>) {
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
}

async fn legacy_cmd_ls(args: &[u8], response: &mut Vec<u8>) {
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
                return;
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
}

async fn legacy_cmd_cat(args: &[u8], response: &mut Vec<u8>) {
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
}

async fn legacy_cmd_write(args: &[u8], response: &mut Vec<u8>) {
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
            let msg = format!("Wrote {} bytes to {}\r\n", content.len(), path);
            response.extend_from_slice(msg.as_bytes());
        }
        Err(e) => {
            let msg = format!("Error writing file: {}\r\n", e);
            response.extend_from_slice(msg.as_bytes());
        }
    }
}

async fn legacy_cmd_append(args: &[u8], response: &mut Vec<u8>) {
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
            let msg = format!("Appended {} bytes to {}\r\n", content.len(), path);
            response.extend_from_slice(msg.as_bytes());
        }
        Err(e) => {
            let msg = format!("Error appending to file: {}\r\n", e);
            response.extend_from_slice(msg.as_bytes());
        }
    }
}

async fn legacy_cmd_rm(args: &[u8], response: &mut Vec<u8>) {
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
            let msg = format!("Removed: {}\r\n", path);
            response.extend_from_slice(msg.as_bytes());
        }
        Err(e) => {
            let msg = format!("Error removing file: {}\r\n", e);
            response.extend_from_slice(msg.as_bytes());
        }
    }
}

async fn legacy_cmd_mkdir(args: &[u8], response: &mut Vec<u8>) {
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
            let msg = format!("Created directory: {}\r\n", path);
            response.extend_from_slice(msg.as_bytes());
        }
        Err(e) => {
            let msg = format!("Error creating directory: {}\r\n", e);
            response.extend_from_slice(msg.as_bytes());
        }
    }
}

async fn legacy_cmd_df(response: &mut Vec<u8>) {
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
}

async fn legacy_cmd_curl(args: &[u8], response: &mut Vec<u8>) {
    if args.is_empty() {
        response.extend_from_slice(b"Usage: curl <url>\r\n");
        response.extend_from_slice(b"Example: curl http://10.0.2.2:8080/\r\n");
        return;
    }

    let url = match core::str::from_utf8(args) {
        Ok(s) => s.trim(),
        Err(_) => {
            response.extend_from_slice(b"Error: Invalid URL\r\n");
            return;
        }
    };

    match http_get(url).await {
        Ok(body) => {
            for line in body.split('\n') {
                response.extend_from_slice(line.as_bytes());
                response.extend_from_slice(b"\r\n");
            }
        }
        Err(e) => {
            let msg = format!("Error: {}\r\n", e);
            response.extend_from_slice(msg.as_bytes());
        }
    }
}

use alloc::string::ToString;

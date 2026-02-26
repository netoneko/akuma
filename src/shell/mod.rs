//! Shell Module
//!
//! Provides async command execution for the SSH shell with an extensible
//! command system. Commands implement the `Command` trait and are registered
//! in a `CommandRegistry`.
//!
//! Supports:
//! - Pipeline execution via the `|` operator
//! - Command chaining via `;` and `&&` operators
//! - Output redirection via `>` and `>>`

pub mod commands;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use crate::ssh::crypto::{split_first_word, trim_bytes};
use crate::process;
use crate::ssh::protocol::SshChannelStream;
use embedded_io_async::Write; // Added this line

// Re-export commonly used items
pub use commands::CommandRegistry;

// ============================================================================
// Shell Context (per-session state)
// ============================================================================

/// Per-session shell context holding state like current working directory
pub struct ShellContext {
    /// Current working directory
    cwd: String,
    /// Use async execution (spawns on user thread, yields properly)
    async_exec: bool,
    /// Use interactive execution for external commands (bidirectional I/O)
    /// Enables real-time stdin/stdout for interactive applications
    interactive_exec: bool,
    /// Environment variables (passed to child processes)
    env: BTreeMap<String, String>,
}

impl ShellContext {
    /// Create a new shell context with root as the working directory
    pub fn new() -> Self {
        let mut env = BTreeMap::new();
        for entry in crate::process::DEFAULT_ENV {
            if let Some((k, v)) = entry.split_once('=') {
                env.insert(String::from(k), String::from(v));
            }
        }
        env.insert(String::from("PWD"), String::from("/"));

        Self {
            cwd: String::from("/"),
            async_exec: crate::config::ENABLE_SSH_ASYNC_EXEC,
            interactive_exec: true,
            env,
        }
    }

    /// Get the current working directory
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Set the current working directory (also updates PWD env var)
    pub fn set_cwd(&mut self, path: &str) {
        self.cwd = String::from(path);
        self.env.insert(String::from("PWD"), String::from(path));
    }

    /// Get all environment variables
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    /// Get a single environment variable
    pub fn get_env(&self, key: &str) -> Option<&str> {
        self.env.get(key).map(|s| s.as_str())
    }

    /// Set an environment variable
    pub fn set_env(&mut self, key: &str, value: &str) {
        self.env.insert(String::from(key), String::from(value));
    }

    /// Remove an environment variable
    pub fn remove_env(&mut self, key: &str) {
        self.env.remove(key);
    }

    /// Convert environment to `KEY=VALUE` vec for process spawning
    pub fn env_as_vec(&self) -> Vec<String> {
        self.env.iter().map(|(k, v)| format!("{}={}", k, v)).collect()
    }

    /// Resolve a path relative to the current working directory
    pub fn resolve_path(&self, path: &str) -> String {
        if path.starts_with('/') {
            normalize_path(path)
        } else {
            let full_path = if self.cwd == "/" {
                format!("/{}", path)
            } else {
                format!("{}/{}", self.cwd, path)
            };
            normalize_path(&full_path)
        }
    }
}

impl Default for ShellContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Normalize a path (resolve . and ..)
fn normalize_path(path: &str) -> String {
    let mut components: Vec<&str> = Vec::new();

    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            c => {
                components.push(c);
            }
        }
    }

    if components.is_empty() {
        String::from("/")
    } else {
        let mut result = String::new();
        for c in components {
            result.push('/');
            result.push_str(c);
        }
        result
    }
}

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
// VecWriter - Write adapter for Vec<u8>
// ============================================================================

/// A writer that collects output into a Vec<u8>
/// Used for capturing command output in pipelines
pub struct VecWriter {
    buffer: Vec<u8>,
}

impl VecWriter {
    /// Create a new empty VecWriter
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Get the collected bytes
    pub fn into_inner(self) -> Vec<u8> {
        self.buffer
    }

    /// Get the collected bytes as a slice
    pub fn as_slice(&self) -> &[u8] {
        &self.buffer
    }
}

impl Default for VecWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl embedded_io_async::ErrorType for VecWriter {
    type Error = core::convert::Infallible;
}

impl embedded_io_async::Write for VecWriter {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

// ============================================================================
// Command Trait
// ============================================================================

/// A command that can be executed by the shell
///
/// Commands are stateless and should be implemented as unit structs.
/// The execute method writes output to a VecWriter and can optionally
/// read from stdin (for pipeline support).
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

    /// Execute the command
    ///
    /// - `args`: command arguments (everything after the command name)
    /// - `stdin`: optional input from a previous command in a pipeline
    /// - `stdout`: writer to send output to (either next command or terminal)
    /// - `ctx`: shell context with per-session state (cwd, etc.)
    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>>;
}

// ============================================================================
// Variable Expansion
// ============================================================================

/// Expand `$VAR`, `${VAR}`, and `~` in a command line byte slice.
/// Single-quoted regions are not expanded. `$$` produces a literal `$`.
pub fn expand_variables(line: &[u8], ctx: &ShellContext) -> Vec<u8> {
    let mut result = Vec::with_capacity(line.len());
    let mut i = 0;
    let mut in_single_quote = false;

    while i < line.len() {
        let b = line[i];

        if b == b'\'' && !in_single_quote {
            in_single_quote = true;
            result.push(b);
            i += 1;
            continue;
        }
        if b == b'\'' && in_single_quote {
            in_single_quote = false;
            result.push(b);
            i += 1;
            continue;
        }
        if in_single_quote {
            result.push(b);
            i += 1;
            continue;
        }

        if b == b'~' && (i == 0 || line[i - 1] == b' ' || line[i - 1] == b'=')
            && (i + 1 >= line.len() || line[i + 1] == b'/' || line[i + 1] == b' ')
        {
            if let Some(home) = ctx.get_env("HOME") {
                result.extend_from_slice(home.as_bytes());
            } else {
                result.push(b'~');
            }
            i += 1;
            continue;
        }

        if b == b'$' {
            if i + 1 < line.len() && line[i + 1] == b'$' {
                result.push(b'$');
                i += 2;
                continue;
            }

            if i + 1 < line.len() && line[i + 1] == b'{' {
                // ${VAR} form
                if let Some(close) = line[i + 2..].iter().position(|&c| c == b'}') {
                    let name = &line[i + 2..i + 2 + close];
                    if let Ok(name_str) = core::str::from_utf8(name) {
                        if let Some(val) = ctx.get_env(name_str) {
                            result.extend_from_slice(val.as_bytes());
                        }
                    }
                    i = i + 2 + close + 1;
                } else {
                    result.push(b'$');
                    i += 1;
                }
                continue;
            }

            // $VAR form
            let start = i + 1;
            let mut end = start;
            while end < line.len()
                && (line[end].is_ascii_alphanumeric() || line[end] == b'_')
            {
                end += 1;
            }
            if end > start {
                let name = &line[start..end];
                if let Ok(name_str) = core::str::from_utf8(name) {
                    if let Some(val) = ctx.get_env(name_str) {
                        result.extend_from_slice(val.as_bytes());
                    }
                }
                i = end;
            } else {
                result.push(b'$');
                i += 1;
            }
            continue;
        }

        result.push(b);
        i += 1;
    }

    result
}

// ============================================================================
// Pipeline Parsing
// ============================================================================

/// Parse a command line into pipeline stages
/// Splits on '|' character, trimming whitespace from each stage
pub fn parse_pipeline(line: &[u8]) -> Vec<&[u8]> {
    let mut stages = Vec::new();
    let mut start = 0;

    for (i, &byte) in line.iter().enumerate() {
        if byte == b'|' {
            let stage = trim_bytes(&line[start..i]);
            if !stage.is_empty() {
                stages.push(stage);
            }
            start = i + 1;
        }
    }

    // Add the last stage
    let stage = trim_bytes(&line[start..]);
    if !stage.is_empty() {
        stages.push(stage);
    }

    stages
}

/// Result of pipeline execution (internal)
enum PipelineResult {
    /// Success with output bytes
    Output(Vec<u8>),
    /// Error with optional message to display
    Error(ShellError, Option<alloc::string::String>),
}

// ============================================================================
// Command Chain Parsing (for ; and && operators)
// ============================================================================

/// Operator between chained commands
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChainOperator {
    /// `;` - Execute next command regardless of previous result
    Semicolon,
    /// `&&` - Execute next command only if previous succeeded
    And,
}

/// A command in a chain, with the operator that follows it
#[derive(Debug)]
pub struct ChainedCommand<'a> {
    /// The command (may be a pipeline with |, >, >>)
    pub command: &'a [u8],
    /// The operator that follows this command (None for the last command)
    pub next_operator: Option<ChainOperator>,
}

/// Parse a command line into chained commands separated by `;` and `&&`
pub fn parse_command_chain(line: &[u8]) -> Vec<ChainedCommand<'_>> {
    let mut commands = Vec::new();
    let mut start = 0;
    let mut i = 0;

    while i < line.len() {
        // Check for && (must check before single &)
        if i + 1 < line.len() && line[i] == b'&' && line[i + 1] == b'&' {
            let cmd = trim_bytes(&line[start..i]);
            if !cmd.is_empty() {
                commands.push(ChainedCommand {
                    command: cmd,
                    next_operator: Some(ChainOperator::And),
                });
            }
            i += 2;
            start = i;
            continue;
        }

        // Check for ;
        if line[i] == b';' {
            let cmd = trim_bytes(&line[start..i]);
            if !cmd.is_empty() {
                commands.push(ChainedCommand {
                    command: cmd,
                    next_operator: Some(ChainOperator::Semicolon),
                });
            }
            i += 1;
            start = i;
            continue;
        }

        i += 1;
    }

    // Add the last command
    let cmd = trim_bytes(&line[start..]);
    if !cmd.is_empty() {
        commands.push(ChainedCommand {
            command: cmd,
            next_operator: None,
        });
    }

    commands
}

/// Check if an executable exists in common binary locations
async fn find_executable(name: &str) -> Option<alloc::string::String> {
    // If it's an absolute path, check it directly
    if name.starts_with('/') {
        if crate::async_fs::exists(name).await {
            if crate::async_fs::list_dir(name).await.is_err() {
                return Some(String::from(name));
            }
        }
        return None;
    }

    // Search in priority order
    let paths = ["/usr/bin", "/bin"];
    for path in paths {
        let bin_path = format!("{}/{}", path, name);
        if crate::async_fs::exists(&bin_path).await {
            // Make sure it's a file, not a directory
            if crate::async_fs::list_dir(&bin_path).await.is_err() {
                return Some(bin_path);
            }
        }
    }
    None
}

/// Parse command line arguments from a byte slice
///
/// Splits on whitespace and converts each argument to a String.
/// Returns a vector of argument strings.
fn parse_args(input: &[u8]) -> Vec<String> {
    let mut args = Vec::new();
    let trimmed = trim_bytes(input);
    
    if trimmed.is_empty() {
        return args;
    }
    
    // Parse with quote handling
    let mut current = Vec::new();
    let mut in_quote: Option<u8> = None; // The quote character we're inside, if any
    
    for &byte in trimmed {
        match in_quote {
            Some(quote_char) => {
                // We're inside a quoted string
                if byte == quote_char {
                    // End of quoted section
                    in_quote = None;
                } else {
                    // Add character to current argument (don't include quotes)
                    current.push(byte);
                }
            }
            None => {
                // Not in a quoted string
                if byte == b'"' || byte == b'\'' {
                    // Start of quoted section
                    in_quote = Some(byte);
                } else if byte.is_ascii_whitespace() {
                    // End of argument
                    if !current.is_empty() {
                        if let Ok(s) = core::str::from_utf8(&current) {
                            args.push(String::from(s));
                        }
                        current.clear();
                    }
                } else {
                    // Regular character
                    current.push(byte);
                }
            }
        }
    }
    
    // Don't forget the last argument
    if !current.is_empty() {
        if let Ok(s) = core::str::from_utf8(&current) {
            args.push(String::from(s));
        }
    }
    
    args
}

use core::sync::atomic::{AtomicBool, Ordering};

/// Flag to indicate whether async execution is available (SSH server running)
/// When false, falls back to synchronous execution for boot-time tests
static ASYNC_EXEC_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable async process execution (call when SSH server starts)
pub fn enable_async_exec() {
    ASYNC_EXEC_ENABLED.store(true, Ordering::Release);
}

/// Check if async execution is enabled
pub fn is_async_exec_enabled() -> bool {
    ASYNC_EXEC_ENABLED.load(Ordering::Acquire)
}

/// Execute an external binary with stdin/stdout (buffered, for pipelines)
async fn execute_external(
    path: &str,
    args: Option<&[&str]>,
    env: Option<&[String]>,
    stdin: Option<&[u8]>,
    cwd: Option<&str>,
    stdout: &mut VecWriter,
    translate_newlines: bool,
    add_exit_code: bool,
) -> Result<(), ShellError> {
    // Use async execution if enabled (SSH context), otherwise sync (test context)
    let result = if is_async_exec_enabled() {
        crate::process::exec_async_cwd(path, args, env, stdin, cwd).await
    } else {
        crate::process::exec_with_io_cwd(path, args, env, stdin, cwd)
    };

    match result {
        Ok((exit_code, process_output)) => {
            // Only convert \n to \r\n for terminal output
            if translate_newlines {
                for &byte in &process_output {
                    if byte == b'\n' {
                        let _ = embedded_io_async::Write::write_all(stdout, b"\r\n").await;
                    } else {
                        let _ = embedded_io_async::Write::write_all(stdout, &[byte]).await;
                    }
                }
            } else {
                let _ = embedded_io_async::Write::write_all(stdout, &process_output).await;
            }

            // Only show exit code if non-zero AND if requested
            if add_exit_code && exit_code != 0 {
                let msg = format!("[exit code: {}]\r\n", exit_code);
                let _ = embedded_io_async::Write::write_all(stdout, msg.as_bytes()).await;
            }
            Ok(())
        }
        Err(e) => {
            let msg = format!("Error: {}\r\n", e);
            let _ = embedded_io_async::Write::write_all(stdout, msg.as_bytes()).await;
            Ok(())
        }
    }
}

/// Execute an external binary with streaming output (for direct SSH output)
pub async fn execute_external_streaming<W>(
    path: &str,
    args: Option<&[&str]>,
    env: Option<&[String]>,
    stdin: Option<&[u8]>,
    cwd: Option<&str>,
    stdout: &mut W,
) -> Result<(), ShellError>
where
    W: embedded_io_async::Write,
{
    match crate::process::exec_streaming_cwd(path, args, env, stdin, cwd, stdout).await {
        Ok(exit_code) => {
            if exit_code != 0 {
                let msg = format!("[exit code: {}]\r\n", exit_code);
                let _ = embedded_io_async::Write::write_all(stdout, msg.as_bytes()).await;
            }
            Ok(())
        },
        Err(e) => {
            let msg = format!("Error: {}\r\n", e);
            let _ = stdout.write_all(msg.as_bytes()).await;
            Err(ShellError::ExecutionFailed("process execution failed"))
        }
    }
}

/// Trait for streams that support interactive (non-blocking) reads
/// This is needed for bidirectional I/O with running processes
pub trait InteractiveRead: embedded_io_async::Read {
    /// Try to read with a very short timeout
    /// Returns 0 if no data available (but not EOF)
    fn try_read_interactive(
        &mut self,
        buf: &mut [u8],
    ) -> impl core::future::Future<Output = Result<usize, Self::Error>>;
}

/// Translate terminal escape sequences to simpler byte equivalents.
/// \x1b[3~ (Delete key) -> \x7f so apps like neatvi delete in insert mode.
fn translate_input_keys(data: &[u8]) -> alloc::vec::Vec<u8> {
    let mut result = alloc::vec::Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if i + 3 < data.len()
            && data[i] == 0x1b
            && data[i + 1] == b'['
            && data[i + 2] == b'3'
            && data[i + 3] == b'~'
        {
            result.push(0x7f);
            i += 4;
        } else {
            result.push(data[i]);
            i += 1;
        }
    }
    result
}

/// Execute an external binary with interactive bidirectional I/O
///
/// This enables truly interactive applications like chat clients that
/// need to read stdin and write stdout in real-time.
///
/// The stream must implement InteractiveRead for non-blocking stdin polling.
pub async fn execute_external_interactive(
    path: &str,
    args: Option<&[&str]>,
    env: Option<&[String]>,
    stdin: Option<&[u8]>,
    cwd: Option<&str>,
    channel_stream: &mut SshChannelStream<'_>,
) -> Result<(), ShellError> {
    use crate::process::spawn_process_with_channel_cwd;
    
    // Spawn process with channel and cwd
    let (thread_id, channel, pid) = match spawn_process_with_channel_cwd(path, args, env, stdin, cwd) {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("Error: {}\r\n", e);
            let _ = channel_stream.write_all(msg.as_bytes()).await;
            return Err(ShellError::ExecutionFailed("process spawn failed"));
        }
    };

    // Set the current process PID and channel in the channel stream
    channel_stream.current_process_pid = Some(pid);
    channel_stream.current_process_channel = Some(channel.clone());

    // Buffer for reading from SSH
    let mut read_buf = [0u8; 256];

    // Interactive loop: poll both directions
    loop {
        // Check for interrupt
        if channel.is_interrupted() {
            break;
        }

        // Check raw mode each iteration — processes like DOOM enable it after init.
        // In raw mode, skip \n → \r\n translation since the process sends its own
        // line endings and binary ANSI escape data that must not be modified.
        let raw_mode = channel.is_raw_mode();

        // 1. Drain process stdout and write to SSH
        if let Some(data) = channel.try_read() {
            crate::safe_print!(128, "[interactive_bridge] read {} bytes from channel\n", data.len());
            if raw_mode {
                // Pass through exactly as written by the process (e.g. escape sequences)
                let _ = channel_stream.write_all(&data).await;
            } else {
                // Perform CRLF translation for cooked mode
                let mut translated = Vec::with_capacity(data.len() + 8);
                for &byte in &data {
                    if byte == b'\n' {
                        translated.extend_from_slice(b"\r\n");
                    } else {
                        translated.push(byte);
                    }
                }
                let _ = channel_stream.write_all(&translated).await;
            }
            let _ = channel_stream.flush().await;
        }

        // 2. Check for process exit
        if channel.has_exited() || crate::threading::is_thread_terminated(thread_id) {
            // Drain remaining output
            while let Some(data) = channel.try_read() {
                if raw_mode {
                    let _ = channel_stream.write_all(&data).await;
                } else {
                    let mut translated = Vec::with_capacity(data.len() + 8);
                    for &byte in &data {
                        if byte == b'\n' {
                            translated.extend_from_slice(b"\r\n");
                        } else {
                            translated.push(byte);
                        }
                    }
                    let _ = channel_stream.write_all(&translated).await;
                }
            }
            let _ = channel_stream.flush().await;
            break;
        }

        // 3. Try to read from SSH (non-blocking)
        match channel_stream.try_read_interactive(&mut read_buf).await {
            Ok(0) => {
                // No data available - continue polling
            }
            Ok(n) => {
                let input_data = &read_buf[..n];
                
                // Check for Ctrl+C
                for &byte in input_data {
                    if byte == 0x03 {
                        channel.set_interrupted();
                    }
                }
                
                // Forward to process stdin using unified helper (UNIFIED I/O).
                // Translate escape sequences: \x1b[3~ (Delete key) -> \x7f
                let translated = translate_input_keys(input_data);
                let _ = process::write_to_process_stdin(pid, &translated);
            }
            Err(_) => {
                // Read error - continue
            }
        }

        // Yield to scheduler
        crate::process::YieldOnce::new().await;
    }

    let exit_code = if channel.is_interrupted() && !channel.has_exited() {
        130
    } else {
        channel.exit_code()
    };

    // Cleanup
    crate::threading::cleanup_terminated();

    if exit_code != 0 && exit_code != 130 {
        let msg = format!("[exit code: {}]\r\n", exit_code);
        let _ = channel_stream.write_all(msg.as_bytes()).await;
    }
    
    // Clear the current process PID and channel in the channel stream
    channel_stream.current_process_pid = None;
    channel_stream.current_process_channel = None;
    
    Ok(())
}

/// Result of checking if a command can be streamed
pub enum StreamableCommand {
    /// Command is a simple external binary that can be streamed
    External(alloc::string::String),
    /// Built-in pkg install with streaming output
    PkgInstall(alloc::string::String),
    /// Command is a builtin or complex (pipes, redirects) - use buffered execution
    Buffered,
    /// Command is exit/quit
    Exit,
}

/// Check if a command line is a simple external binary that can be streamed
///
/// Returns `StreamableCommand::External(path)` if the command is:
/// - A single command (no pipes |)
/// - No output redirection (> or >>)
/// - Not a chain (; or &&)
/// - Not a builtin command
/// - An existing executable in /usr/bin or /bin (or absolute path)
pub async fn check_streamable_command(
    line: &[u8],
    registry: &CommandRegistry,
) -> StreamableCommand {
    let trimmed = trim_bytes(line);
    
    // Check for exit/quit
    if trimmed == b"exit" || trimmed == b"quit" {
        return StreamableCommand::Exit;
    }
    
    // Check for command chaining operators (; or &&)
    for i in 0..trimmed.len() {
        if trimmed[i] == b';' {
            return StreamableCommand::Buffered;
        }
        if i + 1 < trimmed.len() && trimmed[i] == b'&' && trimmed[i + 1] == b'&' {
            return StreamableCommand::Buffered;
        }
    }
    
    // Check for pipes or redirection
    for &byte in trimmed {
        if byte == b'|' || byte == b'>' {
            return StreamableCommand::Buffered;
        }
    }
    
    // Parse the command name
    let (cmd_name, _args) = split_first_word(trimmed);
    
    // Stream pkg install directly to SSH channel for real-time output
    if cmd_name == b"pkg" {
        let (_cmd, args) = split_first_word(trimmed);
        let args_str = core::str::from_utf8(args).unwrap_or("").trim();
        if let Some(packages) = args_str.strip_prefix("install") {
            let packages = packages.trim();
            if !packages.is_empty() {
                return StreamableCommand::PkgInstall(alloc::string::String::from(packages));
            }
        }
    }

    // If built-ins come first, check them now
    if crate::config::SSH_BUILT_INS_FIRST && registry.find(cmd_name).is_some() {
        return StreamableCommand::Buffered;
    }
    
    // Check if it's an external binary in /usr/bin or /bin (or absolute path)
    let cmd_name_str = match core::str::from_utf8(cmd_name) {
        Ok(s) => s,
        Err(_) => return StreamableCommand::Buffered,
    };
    
    if let Some(bin_path) = find_executable(cmd_name_str).await {
        StreamableCommand::External(bin_path)
    } else {
        // Not an external, fall back to built-in check if we haven't already
        StreamableCommand::Buffered
    }
}

/// Execute a simple external command with streaming output
///
/// This handles the common case of running a single external binary
/// with real-time output streaming. For complex commands (pipes, redirects,
/// builtins), use `execute_command_chain` instead.
///
/// Returns `Some(result)` if the command was handled, `None` if it should
/// use buffered execution instead.
pub async fn execute_command_streaming(
    line: &[u8],
    registry: &CommandRegistry,
    ctx: &mut ShellContext,
    channel_stream: &mut SshChannelStream<'_>,
    stdin: Option<&[u8]>,
) -> Option<ChainExecutionResult>
{
    let expanded = expand_variables(line, ctx);
    let line = &expanded[..];

    // Skip interactive check entirely if not enabled - avoid double filesystem lookups
    if !ctx.interactive_exec {
        // Just check for exit command, skip all filesystem operations
        let trimmed = trim_bytes(line);
        if trimmed == b"exit" || trimmed == b"quit" {
            return Some(ChainExecutionResult {
                output: Vec::new(),
                success: true,
                should_exit: true,
            });
        }
        return None; // Fall back to buffered execution
    }

    // Interactive execution enabled - do the full check
    match check_streamable_command(line, registry).await {
        StreamableCommand::External(bin_path) => {
            // Parse args from the command line (kernel adds argv[0] automatically)
            let trimmed = trim_bytes(line);
            let (_cmd_name, args_bytes) = split_first_word(trimmed);
            let arg_strings = parse_args(args_bytes);
            let arg_refs: Vec<&str> = arg_strings.iter().map(|s| s.as_str()).collect();
            let args_slice: Option<&[&str]> = if arg_refs.is_empty() { None } else { Some(&arg_refs) };
            
            // Execute with interactive bidirectional I/O (pass shell's cwd and env)
            let env_vec = ctx.env_as_vec();
            let success = execute_external_interactive(&bin_path, args_slice, Some(&env_vec), stdin, Some(ctx.cwd()), channel_stream).await.is_ok();
            Some(ChainExecutionResult {
                output: Vec::new(), // Output already streamed
                success,
                should_exit: false,
            })
        }
        StreamableCommand::PkgInstall(packages) => {
            let success = commands::net::PKG_CMD
                .install_streaming(&packages, channel_stream, ctx)
                .await
                .is_ok();
            Some(ChainExecutionResult {
                output: Vec::new(),
                success,
                should_exit: false,
            })
        }
        StreamableCommand::Exit => {
            Some(ChainExecutionResult {
                output: Vec::new(),
                success: true,
                should_exit: true,
            })
        }
        StreamableCommand::Buffered => {
            // Fall back to buffered execution
            None
        }
    }
}

/// Execute a pipeline of commands
/// Returns the final output or an error with a message
async fn execute_pipeline_internal(
    stages: &[&[u8]],
    registry: &CommandRegistry,
    ctx: &mut ShellContext,
) -> PipelineResult {
    if stages.is_empty() {
        return PipelineResult::Output(Vec::new());
    }

    let mut stdin_data: Option<Vec<u8>> = None;

    for (i, stage) in stages.iter().enumerate() {
        let (cmd_name, args) = split_first_word(stage);
        let is_last = i == stages.len() - 1;

        // Execute command with stdin from previous stage
        let mut stdout = VecWriter::new();
        let stdin_slice = stdin_data.as_deref();

        // 1. Try built-ins if they come first
        if crate::config::SSH_BUILT_INS_FIRST {
            if let Some(cmd) = registry.find(cmd_name) {
                match cmd.execute(args, stdin_slice, &mut stdout, ctx).await {
                    Ok(()) => {
                        if is_last {
                            return PipelineResult::Output(stdout.into_inner());
                        } else {
                            stdin_data = Some(stdout.into_inner());
                        }
                        continue;
                    }
                    Err(ShellError::Exit) => return PipelineResult::Error(ShellError::Exit, None),
                    Err(ShellError::ExecutionFailed(msg)) => {
                        let error_msg = format!("Error in stage {}: {}\r\n", i + 1, msg);
                        return PipelineResult::Error(ShellError::ExecutionFailed(msg), Some(error_msg));
                    }
                    Err(e) => return PipelineResult::Error(e, None),
                }
            }
        }

        // 2. Try external binaries
        let cmd_name_str = match core::str::from_utf8(cmd_name) {
            Ok(s) => s,
            Err(_) => {
                let msg = "Invalid command name\r\n".into();
                return PipelineResult::Error(ShellError::CommandNotFound, Some(msg));
            }
        };

        if let Some(bin_path) = find_executable(cmd_name_str).await {
            // Found an executable - run it (kernel adds argv[0] automatically)
            let arg_strings = parse_args(args);
            let arg_refs: Vec<&str> = arg_strings.iter().map(|s| s.as_str()).collect();
            let args_slice: Option<&[&str]> = if arg_refs.is_empty() { None } else { Some(&arg_refs) };
            
            // Pass shell's cwd and env to spawned processes
            let env_vec = ctx.env_as_vec();
            let cwd = Some(ctx.cwd());
            let translate_output = is_last;
            let add_exit_code = is_last;
            
            if ctx.async_exec {
                match execute_external(&bin_path, args_slice, Some(&env_vec), stdin_slice, cwd, &mut stdout, translate_output, add_exit_code).await {
                    Ok(()) => {
                        if is_last {
                            return PipelineResult::Output(stdout.into_inner());
                        } else {
                            stdin_data = Some(stdout.into_inner());
                        }
                        continue;
                    }
                    Err(e) => return PipelineResult::Error(e, None),
                }
            } else {           
                match execute_external_streaming(&bin_path, args_slice, Some(&env_vec), stdin_slice, cwd, &mut stdout).await {
                    Ok(()) => {
                        if is_last {
                            return PipelineResult::Output(stdout.into_inner());
                        } else {
                            stdin_data = Some(stdout.into_inner());
                        }
                        continue;
                    }
                    Err(e) => return PipelineResult::Error(e, None),
                }
            }
        }

        // 3. Try built-ins if they haven't been tried yet
        if !crate::config::SSH_BUILT_INS_FIRST {
            if let Some(cmd) = registry.find(cmd_name) {
                match cmd.execute(args, stdin_slice, &mut stdout, ctx).await {
                    Ok(()) => {
                        if is_last {
                            return PipelineResult::Output(stdout.into_inner());
                        } else {
                            stdin_data = Some(stdout.into_inner());
                        }
                        continue;
                    }
                    Err(ShellError::Exit) => return PipelineResult::Error(ShellError::Exit, None),
                    Err(ShellError::ExecutionFailed(msg)) => {
                        let error_msg = format!("Error in stage {}: {}\r\n", i + 1, msg);
                        return PipelineResult::Error(ShellError::ExecutionFailed(msg), Some(error_msg));
                    }
                    Err(e) => return PipelineResult::Error(e, None),
                }
            }
        }

        // Command not found anywhere
        let msg = format!(
            "Unknown command: {}\r\nType 'help' for available commands.\r\n",
            cmd_name_str
        );
        return PipelineResult::Error(ShellError::CommandNotFound, Some(msg));
    }

    PipelineResult::Output(stdin_data.unwrap_or_default())
}

/// Execute a pipeline of commands (public API)
/// Returns the final output or an error
pub async fn execute_pipeline(
    stages: &[&[u8]],
    registry: &CommandRegistry,
    ctx: &mut ShellContext,
) -> Result<Vec<u8>, ShellError> {
    match execute_pipeline_internal(stages, registry, ctx).await {
        PipelineResult::Output(output) => Ok(output),
        PipelineResult::Error(e, _) => Err(e),
    }
}

// ============================================================================
// Output Redirection
// ============================================================================

/// Output redirection mode
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RedirectMode {
    /// No redirection - output to terminal
    None,
    /// Overwrite file (>)
    Overwrite,
    /// Append to file (>>)
    Append,
}

/// Parsed command line with pipeline stages and optional redirection
pub struct ParsedCommandLine<'a> {
    /// Pipeline stages (commands separated by |)
    pub stages: Vec<&'a [u8]>,
    /// Output redirection mode
    pub redirect_mode: RedirectMode,
    /// Target file for redirection (if any)
    pub redirect_target: Option<&'a [u8]>,
}

/// Parse a command line into pipeline stages and redirection
/// Supports: cmd1 | cmd2 > file  or  cmd1 | cmd2 >> file
pub fn parse_command_line(line: &[u8]) -> ParsedCommandLine<'_> {
    // First, check for redirection at the end
    let (pipeline_part, redirect_mode, redirect_target) = parse_redirection(line);

    // Now parse the pipeline part
    let stages = parse_pipeline(pipeline_part);

    ParsedCommandLine {
        stages,
        redirect_mode,
        redirect_target,
    }
}

/// Parse redirection from the end of a command line
/// Returns (pipeline_part, redirect_mode, redirect_target)
fn parse_redirection(line: &[u8]) -> (&[u8], RedirectMode, Option<&[u8]>) {
    // Look for >> first (must check before >)
    for i in 0..line.len().saturating_sub(1) {
        if line[i] == b'>' && line[i + 1] == b'>' {
            let pipeline_part = trim_bytes(&line[..i]);
            let target = trim_bytes(&line[i + 2..]);
            if !target.is_empty() {
                return (pipeline_part, RedirectMode::Append, Some(target));
            }
        }
    }

    // Look for single >
    for i in 0..line.len() {
        if line[i] == b'>' {
            // Make sure it's not >>
            if i + 1 < line.len() && line[i + 1] == b'>' {
                continue;
            }
            let pipeline_part = trim_bytes(&line[..i]);
            let target = trim_bytes(&line[i + 1..]);
            if !target.is_empty() {
                return (pipeline_part, RedirectMode::Overwrite, Some(target));
            }
        }
    }

    (line, RedirectMode::None, None)
}

// ============================================================================
// Unified Command Chain Executor
// ============================================================================

/// Result of executing a command chain
pub struct ChainExecutionResult {
    /// Collected output from all commands
    pub output: Vec<u8>,
    /// Whether the last command succeeded
    pub success: bool,
    /// Whether the shell should exit
    pub should_exit: bool,
}

/// Execute a command chain with proper `;` and `&&` operator handling
///
/// This is the unified executor used by both SSH exec mode and interactive mode.
/// It correctly handles:
/// - `;` operator: Always execute next command regardless of previous result
/// - `&&` operator: Only execute next command if previous succeeded
/// - Output redirection (>, >>)
/// - Pipeline execution (|)
pub async fn execute_command_chain(
    line: &[u8],
    registry: &CommandRegistry,
    ctx: &mut ShellContext,
) -> ChainExecutionResult {
    let expanded = expand_variables(line, ctx);
    let chain = parse_command_chain(&expanded);
    let mut collected_output = Vec::new();
    let mut last_success = true;
    let mut prev_operator: Option<ChainOperator> = None;
    let mut should_exit = false;

    for chained_cmd in &chain {
        // Check if we should skip based on PREVIOUS operator
        if let Some(ChainOperator::And) = prev_operator {
            if !last_success {
                // && and previous failed - skip remaining commands
                break;
            }
        }
        // For ; operator or no previous operator, always continue

        // Check for exit/quit command
        let cmd_trimmed = trim_bytes(chained_cmd.command);
        if cmd_trimmed == b"exit" || cmd_trimmed == b"quit" {
            should_exit = true;
            break;
        }

        // Parse this command for pipeline and redirection
        let parsed = parse_command_line(chained_cmd.command);

        if parsed.stages.is_empty() {
            // Track operator for next iteration
            prev_operator = chained_cmd.next_operator;
            continue;
        }

        // Execute the pipeline

        match execute_pipeline_internal(&parsed.stages, registry, ctx).await {
            PipelineResult::Output(output) => {
                last_success = true;

                // Handle redirection
                match (parsed.redirect_mode, parsed.redirect_target) {
                    (RedirectMode::Overwrite, Some(target)) => {
                        let path = core::str::from_utf8(target).unwrap_or("");
                        match crate::async_fs::write_file(path, &output).await {
                            Ok(()) => {
                                let msg = format!("Wrote {} bytes to {}\r\n", output.len(), path);
                                collected_output.extend_from_slice(msg.as_bytes());
                            }
                            Err(e) => {
                                let msg = format!("Error writing to {}: {}\r\n", path, e);
                                collected_output.extend_from_slice(msg.as_bytes());
                                last_success = false;
                            }
                        }
                    }
                    (RedirectMode::Append, Some(target)) => {
                        let path = core::str::from_utf8(target).unwrap_or("");
                        match crate::async_fs::append_file(path, &output).await {
                            Ok(()) => {
                                let msg =
                                    format!("Appended {} bytes to {}\r\n", output.len(), path);
                                collected_output.extend_from_slice(msg.as_bytes());
                            }
                            Err(e) => {
                                let msg = format!("Error appending to {}: {}\r\n", path, e);
                                collected_output.extend_from_slice(msg.as_bytes());
                                last_success = false;
                            }
                        }
                    }
                    _ => {
                        // No redirection - collect output
                        collected_output.extend_from_slice(&output);
                    }
                }
            }
            PipelineResult::Error(ShellError::Exit, _) => {
                should_exit = true;
                break;
            }
            PipelineResult::Error(ShellError::CommandNotFound, Some(msg)) => {
                collected_output.extend_from_slice(msg.as_bytes());
                last_success = false;
            }
            PipelineResult::Error(ShellError::CommandNotFound, None) => {
                collected_output.extend_from_slice(b"Command not found\r\n");
                last_success = false;
            }
            PipelineResult::Error(ShellError::ExecutionFailed(msg), _) => {
                let error = format!("Error: {}\r\n", msg);
                collected_output.extend_from_slice(error.as_bytes());
                last_success = false;
            }
            PipelineResult::Error(_, Some(msg)) => {
                collected_output.extend_from_slice(msg.as_bytes());
                last_success = false;
            }
            PipelineResult::Error(_, None) => {
                last_success = false;
            }
        }

        // Track operator for next iteration
        prev_operator = chained_cmd.next_operator;
    }

    ChainExecutionResult {
        output: collected_output,
        success: last_success,
        should_exit,
    }
}

// ============================================================================
// Unit Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pipeline_single() {
        let line = b"echo hello";
        let stages = parse_pipeline(line);
        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0], b"echo hello");
    }

    #[test]
    fn test_parse_pipeline_two_stages() {
        let line = b"cat file | grep hello";
        let stages = parse_pipeline(line);
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0], b"cat file");
        assert_eq!(stages[1], b"grep hello");
    }

    #[test]
    fn test_parse_pipeline_three_stages() {
        let line = b"akuma | grep #*####%#**+**%@%**# | head";
        let stages = parse_pipeline(line);
        assert_eq!(stages.len(), 3);
        assert_eq!(stages[0], b"akuma");
        assert_eq!(stages[1], b"grep #*####%#**+**%@%**#");
        assert_eq!(stages[2], b"head");
    }
}

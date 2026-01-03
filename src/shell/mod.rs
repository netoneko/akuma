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
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use crate::ssh::crypto::{split_first_word, trim_bytes};

// Re-export commonly used items
pub use commands::CommandRegistry;

// ============================================================================
// Shell Context (per-session state)
// ============================================================================

/// Per-session shell context holding state like current working directory
pub struct ShellContext {
    /// Current working directory
    cwd: String,
}

impl ShellContext {
    /// Create a new shell context with root as the working directory
    pub fn new() -> Self {
        Self {
            cwd: String::from("/"),
        }
    }

    /// Get the current working directory
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Set the current working directory
    pub fn set_cwd(&mut self, path: &str) {
        self.cwd = String::from(path);
    }

    /// Resolve a path relative to the current working directory
    pub fn resolve_path(&self, path: &str) -> String {
        if path.starts_with('/') {
            // Absolute path
            normalize_path(path)
        } else {
            // Relative path
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

/// Check if an executable exists in /bin
async fn find_executable(name: &str) -> Option<alloc::string::String> {
    let bin_path = format!("/bin/{}", name);
    if crate::async_fs::exists(&bin_path).await {
        // Make sure it's a file, not a directory
        if crate::async_fs::list_dir(&bin_path).await.is_err() {
            return Some(bin_path);
        }
    }
    None
}

/// Execute an external binary with stdin/stdout
async fn execute_external(path: &str, stdin: Option<&[u8]>, stdout: &mut VecWriter) -> Result<(), ShellError> {
    // Set up stdin for the process
    if let Some(input) = stdin {
        crate::syscall::set_stdin(input);
    }

    // Execute the binary
    match crate::process::exec(path) {
        Ok(exit_code) => {
            // Get captured stdout from process
            let process_output = crate::syscall::take_stdout();
            
            // Convert \n to \r\n for terminal
            for &byte in &process_output {
                if byte == b'\n' {
                    let _ = embedded_io_async::Write::write_all(stdout, b"\r\n").await;
                } else {
                    let _ = embedded_io_async::Write::write_all(stdout, &[byte]).await;
                }
            }
            
            // Only show exit code if non-zero
            if exit_code != 0 {
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

/// Execute a pipeline of commands
/// Returns the final output or an error with a message
async fn execute_pipeline_internal(stages: &[&[u8]], registry: &CommandRegistry, ctx: &mut ShellContext) -> PipelineResult {
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

        // First, try built-in commands
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
                Err(ShellError::Exit) => {
                    return PipelineResult::Error(ShellError::Exit, None);
                }
                Err(ShellError::ExecutionFailed(msg)) => {
                    let error_msg = format!("Error in stage {}: {}\r\n", i + 1, msg);
                    return PipelineResult::Error(ShellError::ExecutionFailed(msg), Some(error_msg));
                }
                Err(e) => {
                    return PipelineResult::Error(e, None);
                }
            }
        }

        // Not a built-in - check /bin for an executable
        let cmd_name_str = match core::str::from_utf8(cmd_name) {
            Ok(s) => s,
            Err(_) => {
                let msg = "Invalid command name\r\n".into();
                return PipelineResult::Error(ShellError::CommandNotFound, Some(msg));
            }
        };

        if let Some(bin_path) = find_executable(cmd_name_str).await {
            // Found an executable in /bin - run it
            match execute_external(&bin_path, stdin_slice, &mut stdout).await {
                Ok(()) => {
                    if is_last {
                        return PipelineResult::Output(stdout.into_inner());
                    } else {
                        stdin_data = Some(stdout.into_inner());
                    }
                    continue;
                }
                Err(e) => {
                    return PipelineResult::Error(e, None);
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
pub async fn execute_pipeline(stages: &[&[u8]], registry: &CommandRegistry, ctx: &mut ShellContext) -> Result<Vec<u8>, ShellError> {
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
pub async fn execute_command_chain(line: &[u8], registry: &CommandRegistry, ctx: &mut ShellContext) -> ChainExecutionResult {
    let chain = parse_command_chain(line);
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
                                let msg = format!("Appended {} bytes to {}\r\n", output.len(), path);
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


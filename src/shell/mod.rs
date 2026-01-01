//! Shell Module
//!
//! Provides async command execution for the SSH shell with an extensible
//! command system. Commands implement the `Command` trait and are registered
//! in a `CommandRegistry`. The `ShellSession` handles terminal I/O using
//! nostd-interactive-terminal for line editing and history.
//!
//! Supports pipeline execution via the `|` operator.

pub mod commands;

use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use embedded_io_async::{Read, Write};
use nostd_interactive_terminal::history::{History, HistoryConfig};
use nostd_interactive_terminal::terminal::{ReadLineError, TerminalConfig, TerminalReader};
use nostd_interactive_terminal::writer::TerminalWriter;

use crate::ssh::crypto::{split_first_word, trim_bytes};

// Re-export commonly used items
pub use commands::CommandRegistry;

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
    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
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

/// Result of pipeline execution
enum PipelineResult {
    /// Success with output bytes
    Output(Vec<u8>),
    /// Error with optional message to display
    Error(ShellError, Option<alloc::string::String>),
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
async fn execute_pipeline_internal(stages: &[&[u8]], registry: &CommandRegistry) -> PipelineResult {
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
            match cmd.execute(args, stdin_slice, &mut stdout).await {
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
pub async fn execute_pipeline(stages: &[&[u8]], registry: &CommandRegistry) -> Result<Vec<u8>, ShellError> {
    match execute_pipeline_internal(stages, registry).await {
        PipelineResult::Output(output) => Ok(output),
        PipelineResult::Error(e, _) => Err(e),
    }
}

impl<'a, R: Read, W: Write> ShellSession<'a, R, W> {
    /// Create a new shell session
    pub fn new(reader: &'a mut R, writer: &'a mut W, registry: &'a CommandRegistry) -> Self {
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
        // Get registry reference upfront to avoid borrow conflicts
        let registry = self.registry;

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

            let line = match self
                .reader
                .read_line::<_, _, embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex>(
                    self.reader_inner,
                    &mut writer,
                    None,
                )
                .await
            {
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

            // Check for exit/quit (before pipeline parsing)
            if trimmed == b"exit" || trimmed == b"quit" {
                let _ = writer.writeln("Goodbye!").await;
                return Ok(());
            }

            // Parse and execute pipeline
            let stages = parse_pipeline(trimmed);

            if stages.is_empty() {
                continue;
            }

            // Execute pipeline without holding writer borrow
            let result = execute_pipeline_internal(&stages, registry).await;

            // Now write the result
            match result {
                PipelineResult::Output(output) => {
                    if !output.is_empty() {
                        if let Ok(s) = core::str::from_utf8(&output) {
                            let _ = writer.write_str(s).await;
                        }
                    }
                }
                PipelineResult::Error(ShellError::Exit, _) => return Ok(()),
                PipelineResult::Error(_, Some(msg)) => {
                    let _ = writer.write_str(&msg).await;
                }
                PipelineResult::Error(_, None) => {}
            }
        }
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

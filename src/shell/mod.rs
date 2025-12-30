//! Shell Module
//!
//! Provides async command execution for the SSH shell with an extensible
//! command system. Commands implement the `Command` trait and are registered
//! in a `CommandRegistry`. The `ShellSession` handles terminal I/O using
//! nostd-interactive-terminal for line editing and history.

pub mod commands;

use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use embedded_io_async::{Read, Write};
use nostd_interactive_terminal::terminal::{TerminalConfig, TerminalReader, ReadLineError};
use nostd_interactive_terminal::history::{History, HistoryConfig};
use nostd_interactive_terminal::writer::TerminalWriter;

use crate::ssh_crypto::{split_first_word, trim_bytes};

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
// Command Trait
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


//! Scripting Commands
//!
//! Commands for executing scripts: rhai

use alloc::boxed::Box;
use alloc::format;
use core::future::Future;
use core::pin::Pin;

use embedded_io_async::Write;

use crate::shell::{Command, ShellError, VecWriter};

// ============================================================================
// Rhai Command
// ============================================================================

/// Rhai command - executes a Rhai script from a file
pub struct RhaiCommand;

impl Command for RhaiCommand {
    fn name(&self) -> &'static str {
        "rhai"
    }

    fn description(&self) -> &'static str {
        "Execute a Rhai script"
    }

    fn usage(&self) -> &'static str {
        "rhai <filename>"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let path = core::str::from_utf8(args).unwrap_or("").trim();

            if path.is_empty() {
                let _ = stdout.write(b"Usage: rhai <filename>\r\n").await;
                return Ok(());
            }

            // Check filesystem is initialized
            if !crate::fs::is_initialized() {
                let _ = stdout
                    .write(b"Error: Filesystem not initialized\r\n")
                    .await;
                return Ok(());
            }

            // Read script file (async)
            let script = match crate::async_fs::read_to_string(path).await {
                Ok(s) => s,
                Err(e) => {
                    let msg = format!("Error reading '{}': {}\r\n", path, e);
                    let _ = stdout.write(msg.as_bytes()).await;
                    return Ok(());
                }
            };

            // Execute script (sync, but with operation limits)
            match crate::rhai::run_script(&script) {
                Ok(output) => {
                    if !output.is_empty() {
                        let _ = stdout.write(output.as_bytes()).await;
                    }
                }
                Err(e) => {
                    let _ = stdout.write(e.as_bytes()).await;
                }
            }

            Ok(())
        })
    }
}

/// Static instance
pub static RHAI_CMD: RhaiCommand = RhaiCommand;


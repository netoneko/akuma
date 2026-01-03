//! Exec Command
//!
//! Execute binary programs from the filesystem.

use alloc::boxed::Box;
use alloc::format;
use core::future::Future;
use core::pin::Pin;

use crate::shell::{Command, ShellContext, ShellError, VecWriter};
use crate::ssh::crypto::trim_bytes;

/// Static instance of the exec command
pub static EXEC_CMD: ExecCommand = ExecCommand;

/// Execute binary files
pub struct ExecCommand;

impl Command for ExecCommand {
    fn name(&self) -> &'static str {
        "exec"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["run"]
    }

    fn description(&self) -> &'static str {
        "Execute a binary program"
    }

    fn usage(&self) -> &'static str {
        "exec <path>\n\nExecute the specified binary file.\n\nExample:\n  exec /bin/echo2"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        _ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let args = trim_bytes(args);

            if args.is_empty() {
                let _ = embedded_io_async::Write::write_all(
                    stdout,
                    b"Usage: exec <path>\r\n",
                )
                .await;
                return Ok(());
            }

            // Parse path from args
            let path = match core::str::from_utf8(args) {
                Ok(s) => s.trim(),
                Err(_) => {
                    let _ = embedded_io_async::Write::write_all(
                        stdout,
                        b"Error: Invalid path\r\n",
                    )
                    .await;
                    return Ok(());
                }
            };

            // Execute the binary with per-process I/O
            match crate::process::exec_with_io(path, stdin) {
                Ok((exit_code, process_output)) => {
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
                        let _ = embedded_io_async::Write::write_all(
                            stdout,
                            format!("[exit code: {}]\r\n", exit_code).as_bytes(),
                        )
                        .await;
                    }
                }
                Err(e) => {
                    let _ = embedded_io_async::Write::write_all(
                        stdout,
                        format!("Error: {}\r\n", e).as_bytes(),
                    )
                    .await;
                }
            }

            Ok(())
        })
    }
}


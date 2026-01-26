//! Exec Command
//!
//! Execute binary programs from the filesystem.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use crate::shell::{Command, ShellContext, ShellError, VecWriter};
use crate::ssh::crypto::{split_first_word, trim_bytes};

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
        "exec <path> [args...]\n\nExecute the specified binary file with optional arguments.\n\nExample:\n  exec /bin/hello 5 2"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        _ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let args_trimmed = trim_bytes(args);

            if args_trimmed.is_empty() {
                let _ =
                    embedded_io_async::Write::write_all(stdout, b"Usage: exec <path> [args...]\r\n").await;
                return Ok(());
            }

            // Parse path and remaining args
            let (path_bytes, remaining_args) = split_first_word(args_trimmed);
            let path = match core::str::from_utf8(path_bytes) {
                Ok(s) => s.trim(),
                Err(_) => {
                    let _ = embedded_io_async::Write::write_all(stdout, b"Error: Invalid path\r\n")
                        .await;
                    return Ok(());
                }
            };

            // Parse remaining arguments (kernel adds argv[0] automatically)
            let arg_strings = parse_exec_args(remaining_args);
            let arg_refs: Vec<&str> = arg_strings.iter().map(|s| s.as_str()).collect();
            let args_slice: Option<&[&str]> = if arg_refs.is_empty() { None } else { Some(&arg_refs) };

            // Check if user threads are available for process execution
            let available = crate::threading::user_threads_available();
            if available == 0 {
                let _ = embedded_io_async::Write::write_all(
                    stdout,
                    b"Error: No available threads for process execution\r\n",
                )
                .await;
                return Ok(());
            }

            // Execute the binary asynchronously (non-blocking)
            match crate::process::exec_async(path, args_slice, stdin).await {
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

/// Parse arguments from a byte slice (simple whitespace splitting)
fn parse_exec_args(input: &[u8]) -> Vec<String> {
    let mut args = Vec::new();
    let trimmed = trim_bytes(input);
    
    if trimmed.is_empty() {
        return args;
    }
    
    let mut current = Vec::new();
    for &byte in trimmed {
        if byte.is_ascii_whitespace() {
            if !current.is_empty() {
                if let Ok(s) = core::str::from_utf8(&current) {
                    args.push(String::from(s));
                }
                current.clear();
            }
        } else {
            current.push(byte);
        }
    }
    
    if !current.is_empty() {
        if let Ok(s) = core::str::from_utf8(&current) {
            args.push(String::from(s));
        }
    }
    
    args
}

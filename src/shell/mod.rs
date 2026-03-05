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

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use akuma_exec::process;
use crate::ssh::protocol::SshChannelStream;

pub use akuma_shell::{
    expand_variables, parse_pipeline,
    split_first_word, trim_bytes, translate_input_keys,
    ChainExecutionResult, Command, CommandRegistry,
    InteractiveRead, ShellContext, ShellError,
    StreamableCommand, VecWriter,
};

pub use akuma_shell::exec::{
    check_streamable_command, execute_command_chain, execute_pipeline, ShellBackend,
};

use core::sync::atomic::{AtomicBool, Ordering};

static ASYNC_EXEC_ENABLED: AtomicBool = AtomicBool::new(false);

pub fn enable_async_exec() {
    ASYNC_EXEC_ENABLED.store(true, Ordering::Release);
}

pub fn is_async_exec_enabled() -> bool {
    ASYNC_EXEC_ENABLED.load(Ordering::Acquire)
}

// ============================================================================
// Kernel Shell Backend
// ============================================================================

/// Implements [`ShellBackend`] by delegating to kernel subsystems.
pub struct KernelShellBackend;

impl ShellBackend for KernelShellBackend {
    fn builtins_first(&self) -> bool {
        crate::config::SSH_BUILT_INS_FIRST
    }

    async fn find_executable(&self, name: &str) -> Option<String> {
        find_executable(name).await
    }

    async fn execute_buffered(
        &self,
        path: &str,
        args: Option<&[&str]>,
        env: Option<&[String]>,
        stdin: Option<&[u8]>,
        cwd: Option<&str>,
        stdout: &mut VecWriter,
        translate_newlines: bool,
        add_exit_code: bool,
    ) -> Result<(), ShellError> {
        execute_external(path, args, env, stdin, cwd, stdout, translate_newlines, add_exit_code).await
    }

    async fn execute_streaming<W: embedded_io_async::Write>(
        &self,
        path: &str,
        args: Option<&[&str]>,
        env: Option<&[String]>,
        stdin: Option<&[u8]>,
        cwd: Option<&str>,
        stdout: &mut W,
    ) -> Result<(), ShellError> {
        execute_external_streaming(path, args, env, stdin, cwd, stdout).await
    }

    async fn write_file(&self, path: &str, data: &[u8]) -> Result<(), String> {
        crate::async_fs::write_file(path, data)
            .await
            .map_err(|e| format!("{e}"))
    }

    async fn append_file(&self, path: &str, data: &[u8]) -> Result<(), String> {
        crate::async_fs::append_file(path, data)
            .await
            .map_err(|e| format!("{e}"))
    }
}

// ============================================================================
// Kernel-specific helpers (not extractable)
// ============================================================================

/// Create a new `ShellContext` initialized with kernel defaults.
pub fn new_shell_context() -> ShellContext {
    ShellContext::with_defaults(akuma_exec::process::DEFAULT_ENV, crate::config::ENABLE_SSH_ASYNC_EXEC)
}

async fn find_executable(name: &str) -> Option<String> {
    if name.starts_with('/') {
        if crate::async_fs::exists(name).await {
            if crate::async_fs::list_dir(name).await.is_err() {
                return Some(String::from(name));
            }
        }
        return None;
    }

    let paths = ["/usr/bin", "/bin"];
    for path in paths {
        let bin_path = format!("{path}/{name}");
        if crate::async_fs::exists(&bin_path).await
            && crate::async_fs::list_dir(&bin_path).await.is_err()
        {
            return Some(bin_path);
        }
    }
    None
}

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
    let result = if is_async_exec_enabled() {
        akuma_exec::process::exec_async_cwd(path, args, env, stdin, cwd).await
    } else {
        akuma_exec::process::exec_with_io_cwd(path, args, env, stdin, cwd)
    };

    match result {
        Ok((exit_code, process_output)) => {
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

            if add_exit_code && exit_code != 0 {
                let msg = format!("[exit code: {exit_code}]\r\n");
                let _ = embedded_io_async::Write::write_all(stdout, msg.as_bytes()).await;
            }
            Ok(())
        }
        Err(e) => {
            let msg = format!("Error: {e}\r\n");
            let _ = embedded_io_async::Write::write_all(stdout, msg.as_bytes()).await;
            Ok(())
        }
    }
}

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
    match akuma_exec::process::exec_streaming_cwd(path, args, env, stdin, cwd, stdout).await {
        Ok(exit_code) => {
            if exit_code != 0 {
                let msg = format!("[exit code: {exit_code}]\r\n");
                let _ = embedded_io_async::Write::write_all(stdout, msg.as_bytes()).await;
            }
            Ok(())
        }
        Err(e) => {
            let msg = format!("Error: {e}\r\n");
            let _ = stdout.write_all(msg.as_bytes()).await;
            Err(ShellError::ExecutionFailed("process execution failed"))
        }
    }
}

pub async fn execute_external_interactive(
    path: &str,
    args: Option<&[&str]>,
    env: Option<&[String]>,
    stdin: Option<&[u8]>,
    cwd: Option<&str>,
    channel_stream: &mut SshChannelStream<'_>,
) -> Result<(), ShellError> {
    use akuma_exec::process::spawn_process_with_channel_cwd;
    use embedded_io_async::Write;

    let (thread_id, channel, pid) = match spawn_process_with_channel_cwd(path, args, env, stdin, cwd) {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("Error: {e}\r\n");
            let _ = channel_stream.write_all(msg.as_bytes()).await;
            return Err(ShellError::ExecutionFailed("process spawn failed"));
        }
    };

    channel_stream.current_process_pid = Some(pid);
    channel_stream.current_process_channel = Some(channel.clone());

    let mut read_buf = [0u8; 256];

    loop {
        if channel.is_interrupted() {
            break;
        }

        let raw_mode = channel.is_raw_mode();

        if let Some(data) = channel.try_read() {
            crate::safe_print!(128, "[interactive_bridge] read {} bytes from channel\n", data.len());
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
            let _ = channel_stream.flush().await;
        }

        if channel.has_exited() || akuma_exec::threading::is_thread_terminated(thread_id) {
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

        match channel_stream.try_read_interactive(&mut read_buf).await {
            Ok(0) => {}
            Ok(n) => {
                let input_data = &read_buf[..n];

                for &byte in input_data {
                    if byte == 0x03 {
                        channel.set_interrupted();
                    }
                }

                let translated = translate_input_keys(input_data);
                let _ = process::write_to_process_stdin(pid, &translated);
            }
            Err(_) => {}
        }

        akuma_exec::process::YieldOnce::new().await;
    }

    let exit_code = if channel.is_interrupted() && !channel.has_exited() {
        130
    } else {
        channel.exit_code()
    };

    akuma_exec::threading::cleanup_terminated();

    if exit_code != 0 && exit_code != 130 {
        let msg = format!("[exit code: {exit_code}]\r\n");
        let _ = channel_stream.write_all(msg.as_bytes()).await;
    }

    channel_stream.current_process_pid = None;
    channel_stream.current_process_channel = None;

    Ok(())
}

/// Execute a command with streaming output for interactive sessions.
///
/// This is the main entry point called from the SSH protocol handler.
pub async fn execute_command_streaming_interactive(
    line: &[u8],
    registry: &CommandRegistry,
    ctx: &mut ShellContext,
    channel_stream: &mut SshChannelStream<'_>,
    stdin: Option<&[u8]>,
) -> Option<ChainExecutionResult> {
    let expanded = expand_variables(line, ctx);
    let line = &expanded[..];

    if !ctx.interactive_exec {
        let trimmed = trim_bytes(line);
        if trimmed == b"exit" || trimmed == b"quit" {
            return Some(ChainExecutionResult {
                output: Vec::new(),
                success: true,
                should_exit: true,
            });
        }
        return None;
    }

    let backend = KernelShellBackend;
    match check_streamable_command(line, registry, &backend).await {
        StreamableCommand::External(bin_path) => {
            let trimmed = trim_bytes(line);
            let (_cmd_name, args_bytes) = split_first_word(trimmed);
            let arg_strings = akuma_shell::parse::parse_args(args_bytes);
            let arg_refs: Vec<&str> = arg_strings.iter().map(|s| s.as_str()).collect();
            let args_slice: Option<&[&str]> = if arg_refs.is_empty() { None } else { Some(&arg_refs) };

            let env_vec = ctx.env_as_vec();
            let success = execute_external_interactive(&bin_path, args_slice, Some(&env_vec), stdin, Some(ctx.cwd()), channel_stream).await.is_ok();
            Some(ChainExecutionResult {
                output: Vec::new(),
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
        StreamableCommand::Buffered => None,
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

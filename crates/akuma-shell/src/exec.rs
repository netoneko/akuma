//! Generic pipeline and chain execution, parametrized over [`ShellBackend`].

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::context::ShellContext;
use crate::parse::{
    expand_variables, parse_command_chain, parse_command_line, ChainOperator, RedirectMode,
};
use crate::registry::CommandRegistry;
use crate::types::{ChainExecutionResult, ShellError, StreamableCommand, VecWriter};
use crate::util::{split_first_word, trim_bytes};

/// Abstraction over kernel services required by the shell executor.
///
/// The kernel provides an implementation that wires these to the real
/// filesystem, process subsystem, and config flags.
#[allow(clippy::too_many_arguments)]
pub trait ShellBackend {
    /// Whether to try built-in commands before external binaries.
    fn builtins_first(&self) -> bool;

    /// Check if an executable exists, returning its full path.
    fn find_executable(
        &self,
        name: &str,
    ) -> impl core::future::Future<Output = Option<String>>;

    /// Execute an external binary, capturing output into `stdout`.
    fn execute_buffered(
        &self,
        path: &str,
        args: Option<&[&str]>,
        env: Option<&[String]>,
        stdin: Option<&[u8]>,
        cwd: Option<&str>,
        stdout: &mut VecWriter,
        translate_newlines: bool,
        add_exit_code: bool,
    ) -> impl core::future::Future<Output = Result<(), ShellError>>;

    /// Execute an external binary with streaming I/O.
    fn execute_streaming<W: embedded_io_async::Write>(
        &self,
        path: &str,
        args: Option<&[&str]>,
        env: Option<&[String]>,
        stdin: Option<&[u8]>,
        cwd: Option<&str>,
        stdout: &mut W,
    ) -> impl core::future::Future<Output = Result<(), ShellError>>;

    /// Write (overwrite) a file at `path`.
    fn write_file(
        &self,
        path: &str,
        data: &[u8],
    ) -> impl core::future::Future<Output = Result<(), String>>;

    /// Append to a file at `path`.
    fn append_file(
        &self,
        path: &str,
        data: &[u8],
    ) -> impl core::future::Future<Output = Result<(), String>>;
}

/// Internal pipeline result.
enum PipelineResult {
    Output(Vec<u8>),
    Error(ShellError, Option<String>),
}

/// Execute a pipeline of commands.
#[allow(clippy::future_not_send)]
pub async fn execute_pipeline<B: ShellBackend>(
    stages: &[&[u8]],
    registry: &CommandRegistry,
    ctx: &mut ShellContext,
    backend: &B,
) -> Result<Vec<u8>, ShellError> {
    match execute_pipeline_internal(stages, registry, ctx, backend).await {
        PipelineResult::Output(output) => Ok(output),
        PipelineResult::Error(e, _) => Err(e),
    }
}

#[allow(clippy::future_not_send, clippy::too_many_lines)]
async fn execute_pipeline_internal<B: ShellBackend>(
    stages: &[&[u8]],
    registry: &CommandRegistry,
    ctx: &mut ShellContext,
    backend: &B,
) -> PipelineResult {
    if stages.is_empty() {
        return PipelineResult::Output(Vec::new());
    }

    let mut stdin_data: Option<Vec<u8>> = None;

    for (i, stage) in stages.iter().enumerate() {
        let (cmd_name, args) = split_first_word(stage);
        let is_last = i == stages.len() - 1;

        let mut stdout = VecWriter::new();
        let stdin_slice = stdin_data.as_deref();

        if backend.builtins_first()
            && let Some(cmd) = registry.find(cmd_name)
        {
            match cmd.execute(args, stdin_slice, &mut stdout, ctx).await {
                Ok(()) => {
                    if is_last {
                        return PipelineResult::Output(stdout.into_inner());
                    }
                    stdin_data = Some(stdout.into_inner());
                    continue;
                }
                Err(ShellError::Exit) => {
                    return PipelineResult::Error(ShellError::Exit, None);
                }
                Err(ShellError::ExecutionFailed(msg)) => {
                    let error_msg = format!("Error in stage {}: {msg}\r\n", i + 1);
                    return PipelineResult::Error(
                        ShellError::ExecutionFailed(msg),
                        Some(error_msg),
                    );
                }
                Err(e) => return PipelineResult::Error(e, None),
            }
        }

        let Ok(cmd_name_str) = core::str::from_utf8(cmd_name) else {
            let msg: String = "Invalid command name\r\n".into();
            return PipelineResult::Error(ShellError::CommandNotFound, Some(msg));
        };

        if let Some(bin_path) = backend.find_executable(cmd_name_str).await {
            let arg_strings = crate::parse::parse_args(args);
            let arg_refs: Vec<&str> = arg_strings.iter().map(String::as_str).collect();
            let args_slice: Option<&[&str]> = if arg_refs.is_empty() {
                None
            } else {
                Some(&arg_refs)
            };

            let env_vec = ctx.env_as_vec();
            let cwd = Some(ctx.cwd());
            let translate_output = is_last;
            let add_exit_code = is_last;

            if ctx.async_exec {
                match backend
                    .execute_buffered(
                        &bin_path,
                        args_slice,
                        Some(&env_vec),
                        stdin_slice,
                        cwd,
                        &mut stdout,
                        translate_output,
                        add_exit_code,
                    )
                    .await
                {
                    Ok(()) => {
                        if is_last {
                            return PipelineResult::Output(stdout.into_inner());
                        }
                        stdin_data = Some(stdout.into_inner());
                        continue;
                    }
                    Err(e) => return PipelineResult::Error(e, None),
                }
            }

            match backend
                .execute_streaming(
                    &bin_path,
                    args_slice,
                    Some(&env_vec),
                    stdin_slice,
                    cwd,
                    &mut stdout,
                )
                .await
            {
                Ok(()) => {
                    if is_last {
                        return PipelineResult::Output(stdout.into_inner());
                    }
                    stdin_data = Some(stdout.into_inner());
                    continue;
                }
                Err(e) => return PipelineResult::Error(e, None),
            }
        }

        if !backend.builtins_first()
            && let Some(cmd) = registry.find(cmd_name)
        {
            match cmd.execute(args, stdin_slice, &mut stdout, ctx).await {
                Ok(()) => {
                    if is_last {
                        return PipelineResult::Output(stdout.into_inner());
                    }
                    stdin_data = Some(stdout.into_inner());
                    continue;
                }
                Err(ShellError::Exit) => {
                    return PipelineResult::Error(ShellError::Exit, None);
                }
                Err(ShellError::ExecutionFailed(msg)) => {
                    let error_msg = format!("Error in stage {}: {msg}\r\n", i + 1);
                    return PipelineResult::Error(
                        ShellError::ExecutionFailed(msg),
                        Some(error_msg),
                    );
                }
                Err(e) => return PipelineResult::Error(e, None),
            }
        }

        let msg = format!(
            "Unknown command: {cmd_name_str}\r\nType 'help' for available commands.\r\n"
        );
        return PipelineResult::Error(ShellError::CommandNotFound, Some(msg));
    }

    PipelineResult::Output(stdin_data.unwrap_or_default())
}

/// Execute a command chain with proper `;` and `&&` handling.
///
/// Handles output redirection (`>`, `>>`), pipeline execution (`|`), and
/// command chaining (`;`, `&&`).
#[allow(clippy::too_many_lines, clippy::future_not_send)]
pub async fn execute_command_chain<B: ShellBackend>(
    line: &[u8],
    registry: &CommandRegistry,
    ctx: &mut ShellContext,
    backend: &B,
) -> ChainExecutionResult {
    let expanded = expand_variables(line, ctx);
    let chain = parse_command_chain(&expanded);
    let mut collected_output = Vec::new();
    let mut last_success = true;
    let mut prev_operator: Option<ChainOperator> = None;
    let mut should_exit = false;

    for chained_cmd in &chain {
        if prev_operator == Some(ChainOperator::And) && !last_success {
            break;
        }

        let cmd_trimmed = trim_bytes(chained_cmd.command);
        if cmd_trimmed == b"exit" || cmd_trimmed == b"quit" {
            should_exit = true;
            break;
        }

        let parsed = parse_command_line(chained_cmd.command);

        if parsed.stages.is_empty() {
            prev_operator = chained_cmd.next_operator;
            continue;
        }

        match execute_pipeline_internal(&parsed.stages, registry, ctx, backend).await {
            PipelineResult::Output(output) => {
                last_success = true;

                match (parsed.redirect_mode, parsed.redirect_target) {
                    (RedirectMode::Overwrite, Some(target)) => {
                        let path = core::str::from_utf8(target).unwrap_or("");
                        match backend.write_file(path, &output).await {
                            Ok(()) => {
                                let msg =
                                    format!("Wrote {} bytes to {path}\r\n", output.len());
                                collected_output.extend_from_slice(msg.as_bytes());
                            }
                            Err(e) => {
                                let msg = format!("Error writing to {path}: {e}\r\n");
                                collected_output.extend_from_slice(msg.as_bytes());
                                last_success = false;
                            }
                        }
                    }
                    (RedirectMode::Append, Some(target)) => {
                        let path = core::str::from_utf8(target).unwrap_or("");
                        match backend.append_file(path, &output).await {
                            Ok(()) => {
                                let msg = format!(
                                    "Appended {} bytes to {path}\r\n",
                                    output.len()
                                );
                                collected_output.extend_from_slice(msg.as_bytes());
                            }
                            Err(e) => {
                                let msg = format!("Error appending to {path}: {e}\r\n");
                                collected_output.extend_from_slice(msg.as_bytes());
                                last_success = false;
                            }
                        }
                    }
                    _ => {
                        collected_output.extend_from_slice(&output);
                    }
                }
            }
            PipelineResult::Error(ShellError::Exit, _) => {
                should_exit = true;
                break;
            }
            PipelineResult::Error(ShellError::CommandNotFound, None) => {
                collected_output.extend_from_slice(b"Command not found\r\n");
                last_success = false;
            }
            PipelineResult::Error(ShellError::ExecutionFailed(msg), _) => {
                let error = format!("Error: {msg}\r\n");
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

        prev_operator = chained_cmd.next_operator;
    }

    ChainExecutionResult {
        output: collected_output,
        success: last_success,
        should_exit,
    }
}

/// Check whether a command can be streamed directly to the terminal.
#[allow(clippy::future_not_send)]
pub async fn check_streamable_command<B: ShellBackend>(
    line: &[u8],
    registry: &CommandRegistry,
    backend: &B,
) -> StreamableCommand {
    let trimmed = trim_bytes(line);

    if trimmed == b"exit" || trimmed == b"quit" {
        return StreamableCommand::Exit;
    }

    for i in 0..trimmed.len() {
        if trimmed[i] == b';' {
            return StreamableCommand::Buffered;
        }
        if i + 1 < trimmed.len() && trimmed[i] == b'&' && trimmed[i + 1] == b'&' {
            return StreamableCommand::Buffered;
        }
    }

    for &byte in trimmed {
        if byte == b'|' || byte == b'>' {
            return StreamableCommand::Buffered;
        }
    }

    let (cmd_name, _args) = split_first_word(trimmed);

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

    if backend.builtins_first() && registry.find(cmd_name).is_some() {
        return StreamableCommand::Buffered;
    }

    let Ok(cmd_name_str) = core::str::from_utf8(cmd_name) else {
        return StreamableCommand::Buffered;
    };

    backend
        .find_executable(cmd_name_str)
        .await
        .map_or(StreamableCommand::Buffered, StreamableCommand::External)
}

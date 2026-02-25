//! Shell Module (Userspace Port)
//!
//! Provides async command execution for the SSH shell with an extensible
//! command system.

pub mod commands;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use crate::crypto::{split_first_word, trim_bytes};
use libakuma::*;

// Re-export commonly used items
pub use commands::{CommandRegistry, create_default_registry};

// ============================================================================
// Shell Context (per-session state)
// ============================================================================

pub struct ShellContext {
    cwd: String,
    _async_exec: bool,
    _interactive_exec: bool,
}

impl ShellContext {
    pub fn new() -> Self {
        Self {
            cwd: String::from("/"),
            _async_exec: true,
            _interactive_exec: true,
        }
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn set_cwd(&mut self, path: &str) {
        self.cwd = String::from(path);
    }

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

#[derive(Debug, Clone)]
pub enum ShellError {
    _IoError,
    CommandNotFound,
    ExecutionFailed(&'static str),
    Exit,
    _EndOfFile,
}

// ============================================================================
// VecWriter - Write adapter for Vec<u8>
// ============================================================================

pub struct VecWriter {
    buffer: Vec<u8>,
}

impl VecWriter {
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }
    pub fn into_inner(self) -> Vec<u8> {
        self.buffer
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

pub trait Command: Sync {
    fn name(&self) -> &'static str;
    fn aliases(&self) -> &'static [&'static str] { &[] }
    fn description(&self) -> &'static str;
    fn usage(&self) -> &'static str { "" }
    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>>;
}

// ============================================================================
// Logic Helpers
// ============================================================================

pub fn parse_pipeline(line: &[u8]) -> Vec<&[u8]> {
    let mut stages = Vec::new();
    let mut start = 0;
    for (i, &byte) in line.iter().enumerate() {
        if byte == b'|' {
            let stage = trim_bytes(&line[start..i]);
            if !stage.is_empty() { stages.push(stage); }
            start = i + 1;
        }
    }
    let stage = trim_bytes(&line[start..]);
    if !stage.is_empty() { stages.push(stage); }
    stages
}

enum PipelineResult {
    Output(Vec<u8>),
    Error(ShellError, Option<alloc::string::String>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChainOperator { Semicolon, And }

#[derive(Debug)]
pub struct ChainedCommand<'a> {
    pub command: &'a [u8],
    pub next_operator: Option<ChainOperator>,
}

pub fn parse_command_chain(line: &[u8]) -> Vec<ChainedCommand<'_>> {
    let mut commands = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < line.len() {
        if i + 1 < line.len() && line[i] == b'&' && line[i + 1] == b'&' {
            let cmd = trim_bytes(&line[start..i]);
            if !cmd.is_empty() {
                commands.push(ChainedCommand { command: cmd, next_operator: Some(ChainOperator::And) });
            }
            i += 2; start = i; continue;
        }
        if line[i] == b';' {
            let cmd = trim_bytes(&line[start..i]);
            if !cmd.is_empty() {
                commands.push(ChainedCommand { command: cmd, next_operator: Some(ChainOperator::Semicolon) });
            }
            i += 1; start = i; continue;
        }
        i += 1;
    }
    let cmd = trim_bytes(&line[start..]);
    if !cmd.is_empty() {
        commands.push(ChainedCommand { command: cmd, next_operator: None });
    }
    commands
}

async fn find_executable(name: &str) -> Option<alloc::string::String> {
    if name.starts_with('/') {
        let fd = open(name, open_flags::O_RDONLY);
        if fd >= 0 {
            close(fd);
            return Some(String::from(name));
        }
        return None;
    }

    let paths = ["/usr/bin", "/bin"];
    for path in paths {
        let bin_path = format!("{}/{}", path, name);
        let fd = open(&bin_path, open_flags::O_RDONLY);
        if fd >= 0 {
            close(fd);
            return Some(bin_path);
        }
    }
    None
}

fn parse_args(input: &[u8]) -> Vec<String> {
    let mut args = Vec::new();
    let trimmed = trim_bytes(input);
    if trimmed.is_empty() { return args; }
    let mut current = Vec::new();
    let mut in_quote: Option<u8> = None;
    for &byte in trimmed {
        match in_quote {
            Some(quote_char) => {
                if byte == quote_char { in_quote = None; }
                else { current.push(byte); }
            }
            None => {
                if byte == b'"' || byte == b'\'' { in_quote = Some(byte); }
                else if byte.is_ascii_whitespace() {
                    if !current.is_empty() {
                        if let Ok(s) = core::str::from_utf8(&current) { args.push(String::from(s)); }
                        current.clear();
                    }
                } else { current.push(byte); }
            }
        }
    }
    if !current.is_empty() {
        if let Ok(s) = core::str::from_utf8(&current) { args.push(String::from(s)); }
    }
    args
}

pub struct ChainExecutionResult {
    pub output: Vec<u8>,
    pub success: bool,
    pub should_exit: bool,
}

pub async fn execute_command_chain(
    line: &[u8],
    registry: &CommandRegistry,
    ctx: &mut ShellContext,
) -> ChainExecutionResult {
    let chain = parse_command_chain(line);
    let mut collected_output = Vec::new();
    let mut last_success = true;
    let mut prev_operator: Option<ChainOperator> = None;
    let mut should_exit = false;

    for chained_cmd in &chain {
        if let Some(ChainOperator::And) = prev_operator {
            if !last_success { break; }
        }
        let cmd_trimmed = trim_bytes(chained_cmd.command);
        if cmd_trimmed == b"exit" || cmd_trimmed == b"quit" {
            should_exit = true; break;
        }
        let parsed = parse_command_line(chained_cmd.command);
        if parsed.stages.is_empty() {
            prev_operator = chained_cmd.next_operator; continue;
        }

        match execute_pipeline_internal(&parsed.stages, registry, ctx).await {
            PipelineResult::Output(output) => {
                last_success = true;
                match (parsed.redirect_mode, parsed.redirect_target) {
                    (RedirectMode::Overwrite, Some(target)) => {
                        let path = core::str::from_utf8(target).unwrap_or("");
                        let fd = open(path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
                        if fd >= 0 {
                            write_fd(fd, &output);
                            close(fd);
                            collected_output.extend_from_slice(format!("Wrote {} bytes to {}\r\n", output.len(), path).as_bytes());
                        } else {
                            collected_output.extend_from_slice(format!("Error writing to {}\r\n", path).as_bytes());
                            last_success = false;
                        }
                    }
                    (RedirectMode::Append, Some(target)) => {
                        let path = core::str::from_utf8(target).unwrap_or("");
                        let fd = open(path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_APPEND);
                        if fd >= 0 {
                            write_fd(fd, &output);
                            close(fd);
                            collected_output.extend_from_slice(format!("Appended {} bytes to {}\r\n", output.len(), path).as_bytes());
                        } else {
                            collected_output.extend_from_slice(format!("Error appending to {}\r\n", path).as_bytes());
                            last_success = false;
                        }
                    }
                    _ => { collected_output.extend_from_slice(&output); }
                }
            }
            PipelineResult::Error(ShellError::Exit, _) => { should_exit = true; break; }
            PipelineResult::Error(_, Some(msg)) => {
                collected_output.extend_from_slice(msg.as_bytes());
                last_success = false;
            }
            PipelineResult::Error(_, None) => { last_success = false; }
        }
        prev_operator = chained_cmd.next_operator;
    }

    ChainExecutionResult { output: collected_output, success: last_success, should_exit }
}

async fn execute_pipeline_internal(
    stages: &[&[u8]],
    registry: &CommandRegistry,
    ctx: &mut ShellContext,
) -> PipelineResult {
    if stages.is_empty() { return PipelineResult::Output(Vec::new()); }
    let mut stdin_data: Option<Vec<u8>> = None;

    for (i, stage) in stages.iter().enumerate() {
        let (cmd_name, args) = split_first_word(stage);
        let is_last = i == stages.len() - 1;
        let mut stdout = VecWriter::new();
        let stdin_slice = stdin_data.as_deref();

        if let Some(cmd) = registry.find(cmd_name) {
            match cmd.execute(args, stdin_slice, &mut stdout, ctx).await {
                Ok(()) => {
                    if is_last { return PipelineResult::Output(stdout.into_inner()); }
                    else { stdin_data = Some(stdout.into_inner()); continue; }
                }
                Err(ShellError::Exit) => return PipelineResult::Error(ShellError::Exit, None),
                Err(ShellError::ExecutionFailed(msg)) => return PipelineResult::Error(ShellError::ExecutionFailed(msg), Some(format!("Error: {}\r\n", msg))),
                Err(e) => return PipelineResult::Error(e, None),
            }
        }

        let cmd_name_str = match core::str::from_utf8(cmd_name) {
            Ok(s) => s,
            Err(_) => return PipelineResult::Error(ShellError::CommandNotFound, Some("Invalid command\r\n".into())),
        };

        if let Some(bin_path) = find_executable(cmd_name_str).await {
            let arg_strings = parse_args(args);
            let arg_refs: Vec<&str> = arg_strings.iter().map(|s| s.as_str()).collect();
            
            if let Some(res) = spawn_with_stdin(&bin_path, Some(&arg_refs), stdin_slice) {
                let mut captured = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = read_fd(res.stdout_fd as i32, &mut buf);
                    if n > 0 { captured.extend_from_slice(&buf[..n as usize]); }
                    if let Some(_) = waitpid(res.pid) {
                        while read_fd(res.stdout_fd as i32, &mut buf) > 0 {
                            let n2 = read_fd(res.stdout_fd as i32, &mut buf);
                            if n2 > 0 { captured.extend_from_slice(&buf[..n2 as usize]); }
                            else { break; }
                        }
                        break;
                    }
                    sleep_ms(1);
                }
                if is_last { return PipelineResult::Output(captured); }
                else { stdin_data = Some(captured); continue; }
            } else {
                return PipelineResult::Error(ShellError::ExecutionFailed("Spawn failed"), Some("Failed to spawn process\r\n".into()));
            }
        }

        return PipelineResult::Error(ShellError::CommandNotFound, Some(format!("Unknown command: {}\r\n", cmd_name_str)));
    }
    PipelineResult::Output(stdin_data.unwrap_or_default())
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RedirectMode { None, Overwrite, Append }

pub struct ParsedCommandLine<'a> {
    pub stages: Vec<&'a [u8]>,
    pub redirect_mode: RedirectMode,
    pub redirect_target: Option<&'a [u8]>,
}

pub fn parse_command_line(line: &[u8]) -> ParsedCommandLine<'_> {
    let (pipeline_part, redirect_mode, redirect_target) = parse_redirection(line);
    let stages = parse_pipeline(pipeline_part);
    ParsedCommandLine { stages, redirect_mode, redirect_target }
}

fn parse_redirection(line: &[u8]) -> (&[u8], RedirectMode, Option<&[u8]>) {
    for i in 0..line.len().saturating_sub(1) {
        if line[i] == b'>' && line[i + 1] == b'>' {
            return (trim_bytes(&line[..i]), RedirectMode::Append, Some(trim_bytes(&line[i + 2..])));
        }
    }
    for i in 0..line.len() {
        if line[i] == b'>' {
            return (trim_bytes(&line[..i]), RedirectMode::Overwrite, Some(trim_bytes(&line[i + 1..])));
        }
    }
    (line, RedirectMode::None, None)
}

//! Command-line parsing: pipelines, chains, redirection, variable expansion.

use alloc::string::String;
use alloc::vec::Vec;

use crate::context::ShellContext;
use crate::util::trim_bytes;

/// Operator between chained commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainOperator {
    /// `;` — execute next command regardless of previous result.
    Semicolon,
    /// `&&` — execute next command only if previous succeeded.
    And,
}

/// A command in a chain, with the operator that follows it.
#[derive(Debug)]
pub struct ChainedCommand<'a> {
    pub command: &'a [u8],
    pub next_operator: Option<ChainOperator>,
}

/// Output redirection mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectMode {
    None,
    Overwrite,
    Append,
}

/// Parsed command line with pipeline stages and optional redirection.
pub struct ParsedCommandLine<'a> {
    pub stages: Vec<&'a [u8]>,
    pub redirect_mode: RedirectMode,
    pub redirect_target: Option<&'a [u8]>,
}

/// Parse a command line into pipeline stages (split on `|`).
#[must_use]
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

    let stage = trim_bytes(&line[start..]);
    if !stage.is_empty() {
        stages.push(stage);
    }

    stages
}

/// Parse a command line into chained commands separated by `;` and `&&`.
#[must_use]
pub fn parse_command_chain(line: &[u8]) -> Vec<ChainedCommand<'_>> {
    let mut commands = Vec::new();
    let mut start = 0;
    let mut i = 0;

    while i < line.len() {
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

    let cmd = trim_bytes(&line[start..]);
    if !cmd.is_empty() {
        commands.push(ChainedCommand {
            command: cmd,
            next_operator: None,
        });
    }

    commands
}

/// Parse redirection from the end of a command line.
#[must_use]
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

    for i in 0..line.len() {
        if line[i] == b'>' {
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

/// Parse a command line into pipeline stages and redirection.
#[must_use]
pub fn parse_command_line(line: &[u8]) -> ParsedCommandLine<'_> {
    let (pipeline_part, redirect_mode, redirect_target) = parse_redirection(line);
    let stages = parse_pipeline(pipeline_part);

    ParsedCommandLine {
        stages,
        redirect_mode,
        redirect_target,
    }
}

/// Parse command-line arguments from a byte slice.
///
/// Splits on whitespace with basic quote handling (`"` and `'`).
#[must_use]
pub fn parse_args(input: &[u8]) -> Vec<String> {
    let mut args = Vec::new();
    let trimmed = trim_bytes(input);

    if trimmed.is_empty() {
        return args;
    }

    let mut current = Vec::new();
    let mut in_quote: Option<u8> = None;

    for &byte in trimmed {
        match in_quote {
            Some(quote_char) => {
                if byte == quote_char {
                    in_quote = None;
                } else {
                    current.push(byte);
                }
            }
            None => {
                if byte == b'"' || byte == b'\'' {
                    in_quote = Some(byte);
                } else if byte.is_ascii_whitespace() {
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
        }
    }

    if !current.is_empty()
        && let Ok(s) = core::str::from_utf8(&current)
    {
        args.push(String::from(s));
    }

    args
}

/// Expand `$VAR`, `${VAR}`, and `~` in a command line byte slice.
///
/// Single-quoted regions are not expanded.  `$$` produces a literal `$`.
#[must_use]
pub fn expand_variables(line: &[u8], ctx: &ShellContext) -> Vec<u8> {
    let mut result = Vec::with_capacity(line.len());
    let mut i = 0;
    let mut in_single_quote = false;

    while i < line.len() {
        let b = line[i];

        if b == b'\'' {
            in_single_quote = !in_single_quote;
            result.push(b);
            i += 1;
            continue;
        }
        if in_single_quote {
            result.push(b);
            i += 1;
            continue;
        }

        if b == b'~'
            && (i == 0 || line[i - 1] == b' ' || line[i - 1] == b'=')
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
                if let Some(close) = line[i + 2..].iter().position(|&c| c == b'}') {
                    let name = &line[i + 2..i + 2 + close];
                    if let Ok(name_str) = core::str::from_utf8(name)
                        && let Some(val) = ctx.get_env(name_str)
                    {
                        result.extend_from_slice(val.as_bytes());
                    }
                    i = i + 2 + close + 1;
                } else {
                    result.push(b'$');
                    i += 1;
                }
                continue;
            }

            let start = i + 1;
            let mut end = start;
            while end < line.len()
                && (line[end].is_ascii_alphanumeric() || line[end] == b'_')
            {
                end += 1;
            }
            if end > start {
                let name = &line[start..end];
                if let Ok(name_str) = core::str::from_utf8(name)
                    && let Some(val) = ctx.get_env(name_str)
                {
                    result.extend_from_slice(val.as_bytes());
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

//! Exec Command (Userspace Port)

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use embedded_io_async::Write;
use libakuma::*;
use crate::shell::{Command, ShellContext, ShellError, VecWriter};
use crate::crypto::{split_first_word, trim_bytes};

pub static EXEC_CMD: ExecCommand = ExecCommand;
pub struct ExecCommand;

impl Command for ExecCommand {
    fn name(&self) -> &'static str { "exec" }
    fn aliases(&self) -> &'static [&'static str] { &["run"] }
    fn description(&self) -> &'static str { "Execute a binary program" }
    fn usage(&self) -> &'static str { "exec <path> [args...]" }

    fn execute<'a>(&'a self, args: &'a [u8], stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, _ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let args_trimmed = trim_bytes(args);
            if args_trimmed.is_empty() { let _ = stdout.write(b"Usage: exec <path> [args...]\r\n").await; return Ok(()); }
            let (path_bytes, remaining_args) = split_first_word(args_trimmed);
            let path = core::str::from_utf8(path_bytes).unwrap_or("").trim();
            let arg_strings = parse_exec_args(remaining_args);
            let arg_refs: Vec<&str> = arg_strings.iter().map(|s| s.as_str()).collect();

            if let Some(res) = spawn_with_stdin(path, Some(&arg_refs), stdin) {
                let mut buf = [0u8; 4096];
                loop {
                    let n = read_fd(res.stdout_fd as i32, &mut buf);
                    if n > 0 {
                        for &b in &buf[..n as usize] {
                            if b == b'\n' { let _ = stdout.write(b"\r\n").await; }
                            else { let _ = stdout.write(&[b]).await; }
                        }
                    }
                    if let Some((_, exit_code)) = waitpid(res.pid) {
                        while read_fd(res.stdout_fd as i32, &mut buf) > 0 {
                            let n2 = read_fd(res.stdout_fd as i32, &mut buf);
                            if n2 > 0 {
                                for &b in &buf[..n2 as usize] {
                                    if b == b'\n' { let _ = stdout.write(b"\r\n").await; }
                                    else { let _ = stdout.write(&[b]).await; }
                                }
                            } else { break; }
                        }
                        if exit_code != 0 { let _ = stdout.write(format!("[exit code: {}]\r\n", exit_code).as_bytes()).await; }
                        break;
                    }
                    sleep_ms(1);
                }
            } else { let _ = stdout.write(b"Error: Failed to spawn process\r\n").await; }
            Ok(())
        })
    }
}

fn parse_exec_args(input: &[u8]) -> Vec<String> {
    let mut args = Vec::new();
    let trimmed = trim_bytes(input);
    if trimmed.is_empty() { return args; }
    let mut current = Vec::new();
    for &byte in trimmed {
        if byte.is_ascii_whitespace() {
            if !current.is_empty() {
                if let Ok(s) = core::str::from_utf8(&current) { args.push(String::from(s)); }
                current.clear();
            }
        } else { current.push(byte); }
    }
    if !current.is_empty() {
        if let Ok(s) = core::str::from_utf8(&current) { args.push(String::from(s)); }
    }
    args
}

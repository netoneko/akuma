//! Built-in Shell Commands (Userspace Port)

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use embedded_io_async::Write;
use libakuma::*;
use crate::shell::{Command, ShellContext, ShellError, VecWriter};

// ============================================================================
// ASCII Art (copied from src/akuma.rs)
// ============================================================================

pub const AKUMA_79: &[u8] = b"
                                                                               
                                                                               
                                     .                                         
                                    #@                                         
                                   @@@                                         
                                  @@@@                                         
                                 @@@@@                                         
                                @@@@@@                                         
                               @@@@@@@                                         
                              @@@@@@@@                                         
                             @@@@@@@@@                                         
                            @@@@@@@@@@                                         
                           @@@@@@@@@@@                                         
                          @@@@@@@@@@@@                                         
                         @@@@@@@@@@@@@                                         
                        @@@@@@@@@@@@@@                                         
                       @@@@@@@@@@@@@@@                                         
                      @@@@@@@@@@@@@@@@                                         
                     @@@@@@@@@@@@@@@@@                                         
                    @@@@@@@@@@@@@@@@@@                                         
                   @@@@@@@@@@@@@@@@@@@                                         
                  @@@@@@@@@@@@@@@@@@@@                                         
                 @@@@@@@@@@@@@@@@@@@@@                                         
                @@@@@@@@@@@@@@@@@@@@@@                                         
               @@@@@@@@@@@@@@@@@@@@@@@                                         
              @@@@@@@@@@@@@@@@@@@@@@@@                                         
             @@@@@@@@@@@@@@@@@@@@@@@@@                                         
            @@@@@@@@@@@@@@@@@@@@@@@@@@                                         
           @@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
          @@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
         @@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
        @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
       @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
      @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
     @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
    @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
   @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
  @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@                                         
";

// ============================================================================
// Echo Command
// ============================================================================

pub struct EchoCommand;
impl Command for EchoCommand {
    fn name(&self) -> &'static str { "echo" }
    fn description(&self) -> &'static str { "Echo back text" }
    fn usage(&self) -> &'static str { "echo <text>" }
    fn execute<'a>(&'a self, args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, _ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            if !args.is_empty() { let _ = stdout.write(args).await; }
            let _ = stdout.write(b"\r\n").await;
            Ok(())
        })
    }
}
pub static ECHO_CMD: EchoCommand = EchoCommand;

// ============================================================================
// Akuma Command
// ============================================================================

pub struct AkumaCommand;
impl Command for AkumaCommand {
    fn name(&self) -> &'static str { "akuma" }
    fn description(&self) -> &'static str { "Display ASCII art" }
    fn execute<'a>(&'a self, _args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, _ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            for &byte in AKUMA_79 {
                if byte == b'\n' { let _ = stdout.write(b"\r\n").await; }
                else { let _ = stdout.write(&[byte]).await; }
            }
            Ok(())
        })
    }
}
pub static AKUMA_CMD: AkumaCommand = AkumaCommand;

// ============================================================================
// Stats Command
// ============================================================================

pub struct StatsCommand;
impl Command for StatsCommand {
    fn name(&self) -> &'static str { "stats" }
    fn description(&self) -> &'static str { "Show network statistics" }
    fn execute<'a>(&'a self, _args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, _ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let info = format!("Userspace stats command placeholder.\r\n");
            let _ = stdout.write(info.as_bytes()).await;
            Ok(())
        })
    }
}
pub static STATS_CMD: StatsCommand = StatsCommand;

// ============================================================================
// Free Command
// ============================================================================

pub struct FreeCommand;
impl Command for FreeCommand {
    fn name(&self) -> &'static str { "free" }
    fn aliases(&self) -> &'static [&'static str] { &["mem"] }
    fn description(&self) -> &'static str { "Show memory usage" }
    fn execute<'a>(&'a self, _args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, _ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let usage = memory_usage();
            let total = total_allocated();
            let freed = total_freed();
            let info = format!(
                "Memory Usage:\r\n  Net: {} bytes\r\n  Allocated: {} bytes\r\n  Freed: {} bytes\r\n",
                usage, total, freed
            );
            let _ = stdout.write(info.as_bytes()).await;
            Ok(())
        })
    }
}
pub static FREE_CMD: FreeCommand = FreeCommand;

// ============================================================================
// Help Command
// ============================================================================

pub struct HelpCommand;
impl Command for HelpCommand {
    fn name(&self) -> &'static str { "help" }
    fn description(&self) -> &'static str { "Show this help" }
    fn execute<'a>(&'a self, _args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, _ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let _ = stdout.write(b"Available commands:\r\n").await;
            let _ = stdout.write(b"  echo <text>           - Echo back text\r\n").await;
            let _ = stdout.write(b"  akuma                 - Display ASCII art\r\n").await;
            let _ = stdout.write(b"  stats                 - Show network statistics\r\n").await;
            let _ = stdout.write(b"  free                  - Show memory usage\r\n").await;
            let _ = stdout.write(b"  grep [-iv] <pattern>  - Filter lines by pattern\r\n").await;
            let _ = stdout.write(b"  ps                    - List running processes\r\n").await;
            let _ = stdout.write(b"  clear                 - Clear the terminal screen\r\n").await;
            let _ = stdout.write(b"\r\nNavigation commands:\r\n").await;
            let _ = stdout.write(b"  pwd                   - Print current working directory\r\n").await;
            let _ = stdout.write(b"  cd [path]             - Change current working directory\r\n").await;
            let _ = stdout.write(b"\r\nFilesystem commands:\r\n").await;
            let _ = stdout.write(b"  ls [path]             - List directory contents\r\n").await;
            let _ = stdout.write(b"  cat <file>            - Display file contents\r\n").await;
            let _ = stdout.write(b"  write <file> <text>   - Write text to file\r\n").await;
            let _ = stdout.write(b"  append <file> <text>  - Append text to file\r\n").await;
            let _ = stdout.write(b"  rm <file>             - Remove file\r\n").await;
            let _ = stdout.write(b"  mkdir <dir>           - Create directory\r\n").await;
            let _ = stdout.write(b"\r\nNetwork commands:\r\n").await;
            let _ = stdout.write(b"  pkg install <name>    - Install a package\r\n").await;
            let _ = stdout.write(b"\r\n  help                  - Show this help\r\n").await;
            let _ = stdout.write(b"  quit/exit             - Close connection\r\n").await;
            Ok(())
        })
    }
}
pub static HELP_CMD: HelpCommand = HelpCommand;

// ============================================================================
// Grep Command
// ============================================================================

pub struct GrepCommand;
impl Command for GrepCommand {
    fn name(&self) -> &'static str { "grep" }
    fn description(&self) -> &'static str { "Filter lines by pattern" }
    fn execute<'a>(&'a self, args: &'a [u8], stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, _ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let mut case_insensitive = false;
            let mut invert_match = false;
            let mut pattern: Option<&str> = None;
            let mut file_path: Option<&str> = None;
            let args_str = core::str::from_utf8(args).unwrap_or("");
            let parts: Vec<&str> = args_str.split_whitespace().collect();
            for part in parts {
                if part.starts_with('-') {
                    for c in part[1..].chars() {
                        match c { 'i' => case_insensitive = true, 'v' => invert_match = true, _ => {} }
                    }
                } else if pattern.is_none() { pattern = Some(part); }
                else { file_path = Some(part); }
            }
            let pattern = if let Some(p) = pattern { p } else { let _ = stdout.write(b"Usage: grep [-iv] <pattern> [file]\r\n").await; return Ok(()); };
            let input_data = if let Some(path) = file_path {
                let fd = open(path, open_flags::O_RDONLY);
                if fd < 0 { let _ = stdout.write(format!("Error opening {}: {}\r\n", path, fd).as_bytes()).await; return Ok(()); }
                let mut data = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = read_fd(fd, &mut buf);
                    if n <= 0 { break; }
                    data.extend_from_slice(&buf[..n as usize]);
                }
                close(fd);
                data
            } else if let Some(data) = stdin { data.to_vec() }
            else { let _ = stdout.write(b"grep: no input\r\n").await; return Ok(()); };
            let pattern_lower = pattern.to_lowercase();
            let input_str = core::str::from_utf8(&input_data).unwrap_or("");
            for line in input_str.lines() {
                let matches = if case_insensitive { line.to_lowercase().contains(&pattern_lower) } else { line.contains(pattern) };
                if if invert_match { !matches } else { matches } {
                    let _ = stdout.write(line.as_bytes()).await;
                    let _ = stdout.write(b"\r\n").await;
                }
            }
            Ok(())
        })
    }
}
pub static GREP_CMD: GrepCommand = GrepCommand;

// ============================================================================
// Ps Command
// ============================================================================

pub struct PsCommand;
impl Command for PsCommand {
    fn name(&self) -> &'static str { "ps" }
    fn description(&self) -> &'static str { "List running processes" }
    fn execute<'a>(&'a self, _args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, _ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let mut stats = [ThreadCpuStat::default(); 32];
            let n = get_cpu_stats(&mut stats);
            let _ = stdout.write(b"  PID  TID  STATE     NAME\r\n").await;
            for i in 0..n {
                let s = &stats[i];
                let name = core::str::from_utf8(&s.name).unwrap_or("unknown").trim_matches('\0');
                let line = format!("{:>5} {:>4} {:>8}     {}\r\n", s.pid, s.tid, s.state, name);
                let _ = stdout.write(line.as_bytes()).await;
            }
            Ok(())
        })
    }
}
pub static PS_CMD: PsCommand = PsCommand;

// ============================================================================
// Pwd Command
// ============================================================================

pub struct PwdCommand;
impl Command for PwdCommand {
    fn name(&self) -> &'static str { "pwd" }
    fn description(&self) -> &'static str { "Print current working directory" }
    fn execute<'a>(&'a self, _args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, _ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let _ = stdout.write(getcwd().as_bytes()).await;
            let _ = stdout.write(b"\r\n").await;
            Ok(())
        })
    }
}
pub static PWD_CMD: PwdCommand = PwdCommand;

// ============================================================================
// Cd Command
// ============================================================================

pub struct CdCommand;
impl Command for CdCommand {
    fn name(&self) -> &'static str { "cd" }
    fn description(&self) -> &'static str { "Change current working directory" }
    fn execute<'a>(&'a self, args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, _ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let args_str = core::str::from_utf8(args).unwrap_or("").trim();
            let target = if args_str.is_empty() { "/" } else { args_str };
            if chdir(target) < 0 {
                let _ = stdout.write(format!("cd: {}: No such directory\r\n", target).as_bytes()).await;
            }
            Ok(())
        })
    }
}
pub static CD_CMD: CdCommand = CdCommand;

// ============================================================================
// Uptime Command
// ============================================================================

pub struct UptimeCommand;
impl Command for UptimeCommand {
    fn name(&self) -> &'static str { "uptime" }
    fn description(&self) -> &'static str { "Display system uptime" }
    fn execute<'a>(&'a self, _args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, _ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let us = uptime();
            let sec = us / 1_000_000;
            let hours = sec / 3600;
            let mins = (sec % 3600) / 60;
            let secs = sec % 60;
            let _ = stdout.write(format!("up {}:{:02}:{:02}\r\n", hours, mins, secs).as_bytes()).await;
            Ok(())
        })
    }
}
pub static UPTIME_CMD: UptimeCommand = UptimeCommand;

// ============================================================================
// Kill Command
// ============================================================================

pub struct KillCommand;
impl Command for KillCommand {
    fn name(&self) -> &'static str { "kill" }
    fn description(&self) -> &'static str { "Terminate a process by PID" }
    fn execute<'a>(&'a self, args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, _ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let args_str = core::str::from_utf8(args).unwrap_or("").trim();
            if let Ok(pid) = args_str.parse::<u32>() {
                if kill(pid) < 0 { let _ = stdout.write(b"Kill failed\r\n").await; }
                else { let _ = stdout.write(b"Killed\r\n").await; }
            } else { let _ = stdout.write(b"Usage: kill <pid>\r\n").await; }
            Ok(())
        })
    }
}
pub static KILL_CMD: KillCommand = KillCommand;

// ============================================================================
// Clear Command
// ============================================================================

pub struct ClearCommand;
impl Command for ClearCommand {
    fn name(&self) -> &'static str { "clear" }
    fn description(&self) -> &'static str { "Clear the terminal screen" }
    fn execute<'a>(&'a self, _args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, _ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move { let _ = stdout.write(b"\x1b[2J\x1b[H").await; Ok(()) })
    }
}
pub static CLEAR_CMD: ClearCommand = ClearCommand;

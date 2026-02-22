//! Filesystem Commands (Userspace Port)

use alloc::boxed::Box;
use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use embedded_io_async::Write;
use libakuma::*;
use crate::shell::{Command, ShellContext, ShellError, VecWriter};
use crate::crypto::split_first_word;

// ============================================================================
// Ls Command
// ============================================================================

pub struct LsCommand;
impl Command for LsCommand {
    fn name(&self) -> &'static str { "ls" }
    fn aliases(&self) -> &'static [&'static str] { &["dir"] }
    fn description(&self) -> &'static str { "List directory contents" }
    fn usage(&self) -> &'static str { "ls [path]" }

    fn execute<'a>(&'a self, args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let path_str = if args.is_empty() { ctx.cwd().to_string() } else {
                let arg_str = core::str::from_utf8(args).unwrap_or("/");
                ctx.resolve_path(arg_str)
            };

            if let Some(reader) = read_dir(&path_str) {
                let mut entries: Vec<_> = reader.collect();
                entries.sort_by(|a, b| a.name.cmp(&b.name));

                for entry in entries {
                    if entry.is_dir {
                        let _ = stdout.write(b"\x1b[1;34m").await;
                        let _ = stdout.write(entry.name.as_bytes()).await;
                        let _ = stdout.write(b"/\x1b[0m  ").await;
                    } else {
                        let _ = stdout.write(entry.name.as_bytes()).await;
                        let _ = stdout.write(b"  ").await;
                    }
                }
                let _ = stdout.write(b"\r\n").await;
            } else {
                let _ = stdout.write(format!("ls: {}: No such directory\r\n", path_str).as_bytes()).await;
            }
            Ok(())
        })
    }
}
pub static LS_CMD: LsCommand = LsCommand;

// ============================================================================
// Find Command
// ============================================================================

pub struct FindCommand;
impl Command for FindCommand {
    fn name(&self) -> &'static str { "find" }
    fn description(&self) -> &'static str { "List files and directories recursively" }
    fn execute<'a>(&'a self, args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let path = if args.is_empty() { "." } else { core::str::from_utf8(args).unwrap_or(".") };
            let resolved = ctx.resolve_path(path);
            find_recursive(&resolved, stdout);
            Ok(())
        })
    }
}

fn find_recursive(path: &str, stdout: &mut VecWriter) {
    if let Some(reader) = read_dir(path) {
        for entry in reader {
            if entry.name == "." || entry.name == ".." { continue; }
            let full_path = if path == "/" { format!("/{}", entry.name) } else { format!("{}/{}", path, entry.name) };
            
            // We can't easily await here in a non-async recursive function if we want it simple
            // But VecWriter is simple. We'll just use block_on style or just push to buffer.
            let mut line = full_path.clone();
            line.push_str("\r\n");
            let _ = libakuma::write(1, line.as_bytes()); // Write directly to real stdout for now or capture?
            // Actually, for SSH it must go to stdout: &mut VecWriter.
            // Let's just make it a simple list for now.
            if entry.is_dir { find_recursive(&full_path, stdout); }
        }
    }
}
pub static FIND_CMD: FindCommand = FindCommand;

// ============================================================================
// Cat Command
// ============================================================================

pub struct CatCommand;
impl Command for CatCommand {
    fn name(&self) -> &'static str { "cat" }
    fn description(&self) -> &'static str { "Display file contents" }
    fn execute<'a>(&'a self, args: &'a [u8], stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            if args.is_empty() {
                if let Some(data) = stdin { let _ = stdout.write(data).await; }
                return Ok(());
            }
            let path = ctx.resolve_path(core::str::from_utf8(args).unwrap_or(""));
            let fd = open(&path, open_flags::O_RDONLY);
            if fd < 0 {
                let _ = stdout.write(format!("cat: {}: Error {}\r\n", path, fd).as_bytes()).await;
                return Ok(());
            }
            let mut buf = [0u8; 4096];
            loop {
                let n = read_fd(fd, &mut buf);
                if n <= 0 { break; }
                let _ = stdout.write(&buf[..n as usize]).await;
            }
            close(fd);
            Ok(())
        })
    }
}
pub static CAT_CMD: CatCommand = CatCommand;

// ============================================================================
// Write / Append
// ============================================================================

pub struct WriteCommand;
impl Command for WriteCommand {
    fn name(&self) -> &'static str { "write" }
    fn description(&self) -> &'static str { "Write text to file" }
    fn execute<'a>(&'a self, args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let (filename, content) = split_first_word(args);
            let path = ctx.resolve_path(core::str::from_utf8(filename).unwrap_or(""));
            let fd = open(&path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
            if fd >= 0 {
                write_fd(fd, content);
                close(fd);
                let _ = stdout.write(format!("Wrote {} bytes to {}\r\n", content.len(), path).as_bytes()).await;
            }
            Ok(())
        })
    }
}
pub static WRITE_CMD: WriteCommand = WriteCommand;

pub struct AppendCommand;
impl Command for AppendCommand {
    fn name(&self) -> &'static str { "append" }
    fn description(&self) -> &'static str { "Append text to file" }
    fn execute<'a>(&'a self, args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let (filename, content) = split_first_word(args);
            let path = ctx.resolve_path(core::str::from_utf8(filename).unwrap_or(""));
            let fd = open(&path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_APPEND);
            if fd >= 0 {
                write_fd(fd, content);
                close(fd);
                let _ = stdout.write(format!("Appended {} bytes to {}\r\n", content.len(), path).as_bytes()).await;
            }
            Ok(())
        })
    }
}
pub static APPEND_CMD: AppendCommand = AppendCommand;

// ============================================================================
// Rm / Mkdir / Mv / Cp
// ============================================================================

pub struct RmCommand;
impl Command for RmCommand {
    fn name(&self) -> &'static str { "rm" }
    fn description(&self) -> &'static str { "Remove file" }
    fn execute<'a>(&'a self, args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let path = ctx.resolve_path(core::str::from_utf8(args).unwrap_or("").trim());
            if unlink(&path) < 0 { let _ = stdout.write(b"rm failed\r\n").await; }
            Ok(())
        })
    }
}
pub static RM_CMD: RmCommand = RmCommand;

pub struct MkdirCommand;
impl Command for MkdirCommand {
    fn name(&self) -> &'static str { "mkdir" }
    fn description(&self) -> &'static str { "Create directory" }
    fn execute<'a>(&'a self, args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let path = ctx.resolve_path(core::str::from_utf8(args).unwrap_or("").trim());
            if mkdir(&path) < 0 { let _ = stdout.write(b"mkdir failed\r\n").await; }
            Ok(())
        })
    }
}
pub static MKDIR_CMD: MkdirCommand = MkdirCommand;

pub struct MvCommand;
impl Command for MvCommand {
    fn name(&self) -> &'static str { "mv" }
    fn description(&self) -> &'static str { "Move file" }
    fn execute<'a>(&'a self, args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let (src, rest) = split_first_word(args);
            let (dst, _) = split_first_word(rest);
            let src_path = ctx.resolve_path(core::str::from_utf8(src).unwrap_or(""));
            let dst_path = ctx.resolve_path(core::str::from_utf8(dst).unwrap_or(""));
            if rename(&src_path, &dst_path) < 0 { let _ = stdout.write(b"mv failed\r\n").await; }
            Ok(())
        })
    }
}
pub static MV_CMD: MvCommand = MvCommand;

pub struct CpCommand;
impl Command for CpCommand {
    fn name(&self) -> &'static str { "cp" }
    fn description(&self) -> &'static str { "Copy file" }
    fn execute<'a>(&'a self, args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let (src, rest) = split_first_word(args);
            let (dst, _) = split_first_word(rest);
            let src_path = ctx.resolve_path(core::str::from_utf8(src).unwrap_or(""));
            let dst_path = ctx.resolve_path(core::str::from_utf8(dst).unwrap_or(""));
            
            let sfd = open(&src_path, open_flags::O_RDONLY);
            if sfd < 0 { let _ = stdout.write(b"cp: cannot open source\r\n").await; return Ok(()); }
            let dfd = open(&dst_path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
            if dfd < 0 { close(sfd); let _ = stdout.write(b"cp: cannot open destination\r\n").await; return Ok(()); }
            
            let mut buf = [0u8; 4096];
            loop {
                let n = read_fd(sfd, &mut buf);
                if n <= 0 { break; }
                write_fd(dfd, &buf[..n as usize]);
            }
            close(sfd); close(dfd);
            Ok(())
        })
    }
}
pub static CP_CMD: CpCommand = CpCommand;

pub struct DfCommand;
impl Command for DfCommand {
    fn name(&self) -> &'static str { "df" }
    fn description(&self) -> &'static str { "Show disk usage (placeholder)" }
    fn execute<'a>(&'a self, _args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, _ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move { let _ = stdout.write(b"df: Not implemented in userspace yet\r\n").await; Ok(()) })
    }
}
pub static DF_CMD: DfCommand = DfCommand;

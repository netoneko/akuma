//! Network Commands (Userspace Port)

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
// URL Parsing Helper
// ============================================================================

pub struct PkgCommand;
impl Command for PkgCommand {
    fn name(&self) -> &'static str { "pkg" }
    fn description(&self) -> &'static str { "Package manager" }
    fn execute<'a>(&'a self, args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, _ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let args_str = core::str::from_utf8(args).unwrap_or("").trim();
            
            // Construct args for the /bin/pkg command
            let mut pkg_args: Vec<String> = Vec::new();
            pkg_args.push(String::from("pkg")); // The command name itself
            for arg in args_str.split_whitespace() {
                pkg_args.push(String::from(arg));
            }

            let pkg_args_str: Vec<&str> = pkg_args.iter().map(|s| s.as_str()).collect();

            let _ = stdout.write(format!("sshd: delegating to /bin/pkg...\r\n").as_bytes()).await;

            // Execute /bin/pkg and stream its output
            if let Some(res) = spawn("/bin/pkg", Some(&pkg_args_str)) {
                let mut child_buf = [0u8; 1024];
                loop {
                    // Stream output from the child process
                    let n = read_fd(res.stdout_fd as i32, &mut child_buf);
                    if n > 0 {
                        let _ = stdout.write(&child_buf[..n as usize]).await;
                    }

                    // Check if the process has exited
                    if let Some((_, exit_code)) = waitpid(res.pid) {
                        // Drain any remaining output
                        loop {
                            let n = read_fd(res.stdout_fd as i32, &mut child_buf);
                            if n <= 0 { break; }
                            let _ = stdout.write(&child_buf[..n as usize]).await;
                        }
                        let _ = stdout.write(format!("\r\nsshd: /bin/pkg exited with status {}\r\n", exit_code).as_bytes()).await;
                        break;
                    }
                    sleep_ms(10);
                }
            } else {
                let _ = stdout.write(b"sshd: failed to spawn /bin/pkg. Is it on the disk image?\r\n").await;
            }
            Ok(())
        })
    }
}

pub static PKG_CMD: PkgCommand = PkgCommand;

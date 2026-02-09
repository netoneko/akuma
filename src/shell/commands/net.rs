//! Network Commands (Stubbed for SmolNet)
//!
//! Commands for network operations: curl, nslookup

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use crate::shell::{Command, ShellContext, ShellError, VecWriter};

pub struct CurlCommand;

impl Command for CurlCommand {
    fn name(&self) -> &'static str { "curl" }
    fn description(&self) -> &'static str { "HTTP/HTTPS GET request (stub)" }
    fn usage(&self) -> &'static str { "curl <url>" }

    fn execute<'a>(
        &'a self,
        _args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        _ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let _ = embedded_io_async::Write::write(stdout, b"curl is temporarily disabled during smoltcp migration\r\n").await;
            Ok(())
        })
    }
}

pub static CURL_CMD: CurlCommand = CurlCommand;

pub struct NslookupCommand;

impl Command for NslookupCommand {
    fn name(&self) -> &'static str { "nslookup" }
    fn description(&self) -> &'static str { "DNS lookup (stub)" }
    fn usage(&self) -> &'static str { "nslookup <hostname>" }

    fn execute<'a>(
        &'a self,
        _args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        _ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let _ = embedded_io_async::Write::write(stdout, b"nslookup is temporarily disabled during smoltcp migration\r\n").await;
            Ok(())
        })
    }
}

pub static NSLOOKUP_CMD: NslookupCommand = NslookupCommand;

pub struct PkgCommand;

impl Command for PkgCommand {
    fn name(&self) -> &'static str { "pkg" }
    fn description(&self) -> &'static str { "Package manager (stub)" }
    fn usage(&self) -> &'static str { "pkg install <package>" }

    fn execute<'a>(
        &'a self,
        _args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        _ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let _ = embedded_io_async::Write::write(stdout, b"pkg is temporarily disabled during smoltcp migration\r\n").await;
            Ok(())
        })
    }
}

pub static PKG_CMD: PkgCommand = PkgCommand;
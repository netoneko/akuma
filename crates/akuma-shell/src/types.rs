//! Core shell types and traits.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use crate::context::ShellContext;

/// Errors that can occur during shell operations.
#[derive(Debug, Clone)]
pub enum ShellError {
    IoError,
    CommandNotFound,
    ExecutionFailed(&'static str),
    Exit,
    EndOfFile,
}

/// A writer that collects output into a `Vec<u8>`.
///
/// Used for capturing command output in pipelines.
pub struct VecWriter {
    buffer: Vec<u8>,
}

impl VecWriter {
    #[must_use]
    pub const fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    #[must_use]
    pub fn into_inner(self) -> Vec<u8> {
        self.buffer
    }

    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.buffer
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

/// A shell command that can be executed.
///
/// Commands are stateless and should be implemented as unit structs.
pub trait Command: Sync {
    fn name(&self) -> &'static str;

    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }

    fn description(&self) -> &'static str;

    fn usage(&self) -> &'static str {
        ""
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>>;
}

/// Trait for streams that support interactive (non-blocking) reads.
///
/// Needed for bidirectional I/O with running processes.
pub trait InteractiveRead: embedded_io_async::Read {
    fn try_read_interactive(
        &mut self,
        buf: &mut [u8],
    ) -> impl Future<Output = Result<usize, Self::Error>>;
}

/// Result of checking whether a command can be streamed directly.
pub enum StreamableCommand {
    External(String),
    PkgInstall(String),
    Buffered,
    Exit,
}

/// Result of executing a command chain.
pub struct ChainExecutionResult {
    pub output: Vec<u8>,
    pub success: bool,
    pub should_exit: bool,
}

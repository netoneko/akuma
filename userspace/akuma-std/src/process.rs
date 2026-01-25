//! Process management for akuma

use crate::io::{self, Write};

/// Terminates the process with the specified exit code
pub fn exit(code: i32) -> ! {
    libakuma::exit(code)
}

/// Terminates the process in an abnormal fashion
pub fn abort() -> ! {
    libakuma::exit(134) // 128 + SIGABRT(6)
}

/// Returns the process ID
pub fn id() -> u32 {
    libakuma::getpid()
}

/// A process exit code
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitCode(i32);

impl ExitCode {
    pub const SUCCESS: ExitCode = ExitCode(0);
    pub const FAILURE: ExitCode = ExitCode(1);

    pub fn from_raw(code: i32) -> Self {
        ExitCode(code)
    }
}

impl From<u8> for ExitCode {
    fn from(code: u8) -> Self {
        ExitCode(code as i32)
    }
}

/// Exit status of a child process
#[derive(Clone, Copy, Debug)]
pub struct ExitStatus {
    code: Option<i32>,
}

impl ExitStatus {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    pub fn code(&self) -> Option<i32> {
        self.code
    }
}

impl core::fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.code {
            Some(code) => write!(f, "exit code: {}", code),
            None => write!(f, "signal"),
        }
    }
}

/// Standard output of a child process
pub struct ChildStdout {
    fd: u32,
}

impl io::Read for ChildStdout {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = libakuma::read(self.fd as u64, buf);
        if n < 0 {
            Err(io::Error::from_raw_os_error((-n) as i32))
        } else {
            Ok(n as usize)
        }
    }
}

/// Standard error of a child process (same as stdout in akuma)
pub struct ChildStderr {
    fd: u32,
}

impl io::Read for ChildStderr {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = libakuma::read(self.fd as u64, buf);
        if n < 0 {
            Err(io::Error::from_raw_os_error((-n) as i32))
        } else {
            Ok(n as usize)
        }
    }
}

/// Standard input to a child process
pub struct ChildStdin {
    // Not implemented yet
}

impl io::Write for ChildStdin {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(buf.len()) // Stub
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Representation of a running or exited child process
pub struct Child {
    pub pid: u32,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
    pub stdin: Option<ChildStdin>,
}

impl Child {
    /// Wait for the child to exit and get its status
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some((_, exit_code)) = libakuma::waitpid(self.pid) {
                return Ok(ExitStatus { code: Some(exit_code) });
            }
            // Yield and try again
            libakuma::sleep_ms(10);
        }
    }

    /// Check if child has exited without blocking
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match libakuma::waitpid(self.pid) {
            Some((_, exit_code)) => Ok(Some(ExitStatus { code: Some(exit_code) })),
            None => Ok(None),
        }
    }

    /// Kill the child process
    pub fn kill(&mut self) -> io::Result<()> {
        let result = libakuma::kill(self.pid);
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(-result))
        }
    }

    /// Get the child's process ID
    pub fn id(&self) -> u32 {
        self.pid
    }
}

/// Builder for spawning processes
pub struct Command {
    program: alloc::string::String,
    args: alloc::vec::Vec<alloc::string::String>,
}

impl Command {
    /// Create a new Command for launching the program at `program`
    pub fn new<S: AsRef<str>>(program: S) -> Command {
        Command {
            program: alloc::string::String::from(program.as_ref()),
            args: alloc::vec::Vec::new(),
        }
    }

    /// Add an argument
    pub fn arg<S: AsRef<str>>(&mut self, arg: S) -> &mut Command {
        self.args.push(alloc::string::String::from(arg.as_ref()));
        self
    }

    /// Add multiple arguments
    pub fn args<I, S>(&mut self, args: I) -> &mut Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for arg in args {
            self.arg(arg);
        }
        self
    }

    /// Spawn the process
    pub fn spawn(&mut self) -> io::Result<Child> {
        let args_refs: alloc::vec::Vec<&str> = self.args.iter().map(|s| s.as_str()).collect();
        let args_opt = if args_refs.is_empty() {
            None
        } else {
            Some(args_refs.as_slice())
        };

        match libakuma::spawn(&self.program, args_opt) {
            Some(result) => Ok(Child {
                pid: result.pid,
                stdout: Some(ChildStdout { fd: result.stdout_fd }),
                stderr: None,
                stdin: None,
            }),
            None => Err(io::Error::new(io::ErrorKind::NotFound, "spawn failed")),
        }
    }

    /// Run the command and wait for it to complete
    pub fn output(&mut self) -> io::Result<Output> {
        let mut child = self.spawn()?;
        
        // Read all stdout
        let mut stdout = alloc::vec::Vec::new();
        if let Some(ref mut child_stdout) = child.stdout {
            let mut buf = [0u8; 1024];
            loop {
                match io::Read::read(child_stdout, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => stdout.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        libakuma::sleep_ms(10);
                    }
                    Err(_) => break,
                }
            }
        }

        let status = child.wait()?;
        
        Ok(Output {
            status,
            stdout,
            stderr: alloc::vec::Vec::new(),
        })
    }

    /// Run the command and return its status
    pub fn status(&mut self) -> io::Result<ExitStatus> {
        let mut child = self.spawn()?;
        child.wait()
    }
}

/// Output of a finished process
pub struct Output {
    pub status: ExitStatus,
    pub stdout: alloc::vec::Vec<u8>,
    pub stderr: alloc::vec::Vec<u8>,
}

/// Stdio configuration (stub for compatibility)
pub struct Stdio(u8);

impl Stdio {
    pub fn inherit() -> Stdio {
        Stdio(0)
    }

    pub fn null() -> Stdio {
        Stdio(1)
    }

    pub fn piped() -> Stdio {
        Stdio(2)
    }
}

/// Termination trait for main function
pub trait Termination {
    fn report(self) -> ExitCode;
}

impl Termination for () {
    fn report(self) -> ExitCode {
        ExitCode::SUCCESS
    }
}

impl Termination for ExitCode {
    fn report(self) -> ExitCode {
        self
    }
}

impl<E: core::fmt::Debug> Termination for Result<(), E> {
    fn report(self) -> ExitCode {
        match self {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                let _ = writeln!(io::stderr(), "Error: {:?}", e);
                ExitCode::FAILURE
            }
        }
    }
}

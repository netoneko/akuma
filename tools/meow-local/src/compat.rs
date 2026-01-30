//! Compatibility layer for running meow on native OS (macOS/Linux)
//!
//! Provides APIs similar to libakuma but implemented using std library.

use std::io::{Read, Write};
use std::time::{Duration, Instant};
use std::sync::OnceLock;
use std::os::unix::io::AsRawFd;

// ============================================================================
// Printing
// ============================================================================

pub fn print(s: &str) {
    let _ = std::io::stdout().write_all(s.as_bytes());
    let _ = std::io::stdout().flush();
}

// ============================================================================
// Time functions
// ============================================================================

static START_TIME: OnceLock<Instant> = OnceLock::new();

fn get_start_time() -> &'static Instant {
    START_TIME.get_or_init(Instant::now)
}

pub fn uptime() -> u64 {
    get_start_time().elapsed().as_micros() as u64
}

pub fn sleep_ms(milliseconds: u64) {
    std::thread::sleep(Duration::from_millis(milliseconds));
}

// ============================================================================
// Terminal Raw Mode (for escape key detection)
// ============================================================================

/// Original terminal settings, saved when entering raw mode
static ORIGINAL_TERMIOS: OnceLock<libc::termios> = OnceLock::new();

/// Enter raw mode - allows reading individual key presses
pub fn enter_raw_mode() -> bool {
    unsafe {
        let fd = std::io::stdin().as_raw_fd();
        let mut termios: libc::termios = std::mem::zeroed();
        
        if libc::tcgetattr(fd, &mut termios) != 0 {
            return false;
        }
        
        // Save original settings
        let _ = ORIGINAL_TERMIOS.set(termios);
        
        // Modify for raw mode
        termios.c_lflag &= !(libc::ICANON | libc::ECHO);
        termios.c_cc[libc::VMIN] = 0;  // Non-blocking
        termios.c_cc[libc::VTIME] = 0; // No timeout
        
        libc::tcsetattr(fd, libc::TCSANOW, &termios) == 0
    }
}

/// Exit raw mode - restore original terminal settings
pub fn exit_raw_mode() {
    if let Some(original) = ORIGINAL_TERMIOS.get() {
        unsafe {
            let fd = std::io::stdin().as_raw_fd();
            libc::tcsetattr(fd, libc::TCSANOW, original);
        }
    }
}

/// Check if escape key was pressed (non-blocking)
/// Returns true if escape (0x1B) was detected
pub fn check_escape_pressed() -> bool {
    let mut buf = [0u8; 8];
    let stdin = std::io::stdin();
    let mut handle = stdin.lock();
    
    // Try to read without blocking
    match handle.read(&mut buf) {
        Ok(n) if n > 0 => {
            // Check for escape key (0x1B)
            buf[..n].contains(&0x1B)
        }
        _ => false,
    }
}

// ============================================================================
// Networking
// ============================================================================

pub mod net {
    use std::io::{Read as IoRead, Write as IoWrite};
    use std::net::{TcpStream as StdTcpStream, ToSocketAddrs};
    use std::time::Duration;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ErrorKind {
        NotFound,
        PermissionDenied,
        ConnectionRefused,
        ConnectionReset,
        ConnectionAborted,
        NotConnected,
        AddrInUse,
        AddrNotAvailable,
        BrokenPipe,
        AlreadyExists,
        WouldBlock,
        InvalidInput,
        InvalidData,
        TimedOut,
        WriteZero,
        Interrupted,
        UnexpectedEof,
        Other,
    }

    impl From<std::io::ErrorKind> for ErrorKind {
        fn from(kind: std::io::ErrorKind) -> Self {
            match kind {
                std::io::ErrorKind::NotFound => ErrorKind::NotFound,
                std::io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
                std::io::ErrorKind::ConnectionRefused => ErrorKind::ConnectionRefused,
                std::io::ErrorKind::ConnectionReset => ErrorKind::ConnectionReset,
                std::io::ErrorKind::ConnectionAborted => ErrorKind::ConnectionAborted,
                std::io::ErrorKind::NotConnected => ErrorKind::NotConnected,
                std::io::ErrorKind::AddrInUse => ErrorKind::AddrInUse,
                std::io::ErrorKind::AddrNotAvailable => ErrorKind::AddrNotAvailable,
                std::io::ErrorKind::BrokenPipe => ErrorKind::BrokenPipe,
                std::io::ErrorKind::AlreadyExists => ErrorKind::AlreadyExists,
                std::io::ErrorKind::WouldBlock => ErrorKind::WouldBlock,
                std::io::ErrorKind::InvalidInput => ErrorKind::InvalidInput,
                std::io::ErrorKind::InvalidData => ErrorKind::InvalidData,
                std::io::ErrorKind::TimedOut => ErrorKind::TimedOut,
                std::io::ErrorKind::WriteZero => ErrorKind::WriteZero,
                std::io::ErrorKind::Interrupted => ErrorKind::Interrupted,
                std::io::ErrorKind::UnexpectedEof => ErrorKind::UnexpectedEof,
                _ => ErrorKind::Other,
            }
        }
    }

    #[derive(Debug)]
    pub struct Error {
        pub kind: ErrorKind,
    }

    impl From<std::io::Error> for Error {
        fn from(e: std::io::Error) -> Self {
            Error {
                kind: e.kind().into(),
            }
        }
    }

    /// TCP stream wrapper
    pub struct TcpStream {
        inner: StdTcpStream,
    }

    impl TcpStream {
        pub fn connect(addr: &str) -> Result<Self, Error> {
            let stream = if let Ok(addrs) = addr.to_socket_addrs() {
                let addrs: Vec<_> = addrs.collect();
                if addrs.is_empty() {
                    return Err(Error {
                        kind: ErrorKind::InvalidInput,
                    });
                }
                StdTcpStream::connect(&addrs[..]).map_err(Error::from)?
            } else {
                return Err(Error {
                    kind: ErrorKind::InvalidInput,
                });
            };

            // Set a read timeout to make reads non-blocking-ish
            stream.set_read_timeout(Some(Duration::from_millis(100))).map_err(Error::from)?;
            stream.set_nonblocking(false).map_err(Error::from)?;

            Ok(TcpStream { inner: stream })
        }

        pub fn read(&self, buf: &mut [u8]) -> Result<usize, Error> {
            let mut stream = &self.inner;
            match stream.read(buf) {
                Ok(n) => Ok(n),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    Err(Error { kind: ErrorKind::WouldBlock })
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    Err(Error { kind: ErrorKind::TimedOut })
                }
                Err(e) => Err(e.into()),
            }
        }

        pub fn write_all(&self, buf: &[u8]) -> Result<(), Error> {
            let mut stream = &self.inner;
            stream.write_all(buf)?;
            Ok(())
        }
    }
}

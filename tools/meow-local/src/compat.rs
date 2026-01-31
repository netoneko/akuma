//! Compatibility layer for running meow on native OS (macOS/Linux)
//!
//! Provides APIs similar to libakuma but implemented using std library.

use std::io::Write;
use std::time::{Duration, Instant};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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
// Escape Key Detection (using background thread)
// ============================================================================

/// Cancellation token that can be checked from the main thread
/// while a background thread monitors for escape key
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl CancelToken {
    /// Start monitoring for escape key in background
    pub fn new() -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::new(AtomicBool::new(false));
        
        let cancelled_clone = cancelled.clone();
        let stop_clone = stop_flag.clone();
        
        let handle = std::thread::spawn(move || {
            use std::os::unix::io::AsRawFd;
            let fd = std::io::stdin().as_raw_fd();
            
            let mut buf = [0u8; 8];
            while !stop_clone.load(Ordering::Relaxed) {
                // Use poll() to check if stdin has data, without modifying fd flags
                let mut pollfd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                
                unsafe {
                    // Poll with 50ms timeout
                    let ret = libc::poll(&mut pollfd, 1, 50);
                    if ret > 0 && (pollfd.revents & libc::POLLIN) != 0 {
                        // Data available, read it
                        let n = libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
                        if n > 0 {
                            // Check for escape key (0x1B)
                            if buf[..n as usize].contains(&0x1B) {
                                cancelled_clone.store(true, Ordering::Relaxed);
                                break;
                            }
                        }
                    }
                }
            }
        });
        
        CancelToken { 
            cancelled, 
            stop_flag,
            handle: Some(handle),
        }
    }
    
    /// Check if cancellation was requested
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

impl Drop for CancelToken {
    fn drop(&mut self) {
        // Signal the background thread to stop
        self.stop_flag.store(true, Ordering::Relaxed);
        // Wait for thread to finish (with timeout via the poll in the thread)
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

// ============================================================================
// Networking
// ============================================================================

pub mod net {
    use std::io::{Read as IoRead, Write as IoWrite};
    use std::net::{TcpStream as StdTcpStream, ToSocketAddrs};
    use std::time::Duration;
    use native_tls::{TlsConnector, TlsStream};
    use std::cell::RefCell;

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
        TlsError,
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
        pub message: Option<String>,
    }

    impl From<std::io::Error> for Error {
        fn from(e: std::io::Error) -> Self {
            Error {
                kind: e.kind().into(),
                message: Some(e.to_string()),
            }
        }
    }

    impl From<native_tls::Error> for Error {
        fn from(e: native_tls::Error) -> Self {
            Error {
                kind: ErrorKind::TlsError,
                message: Some(e.to_string()),
            }
        }
    }

    impl<S> From<native_tls::HandshakeError<S>> for Error {
        fn from(e: native_tls::HandshakeError<S>) -> Self {
            let message = match e {
                native_tls::HandshakeError::Failure(err) => err.to_string(),
                native_tls::HandshakeError::WouldBlock(_) => "TLS handshake would block".to_string(),
            };
            Error {
                kind: ErrorKind::TlsError,
                message: Some(message),
            }
        }
    }

    /// Stream type that can be either plain TCP or TLS
    enum StreamInner {
        Plain(StdTcpStream),
        Tls(TlsStream<StdTcpStream>),
    }

    /// TCP/TLS stream wrapper - supports both HTTP and HTTPS
    pub struct Stream {
        inner: RefCell<StreamInner>,
    }

    impl Stream {
        /// Connect to a plain TCP socket (HTTP)
        pub fn connect(addr: &str) -> Result<Self, Error> {
            let stream = if let Ok(addrs) = addr.to_socket_addrs() {
                let addrs: Vec<_> = addrs.collect();
                if addrs.is_empty() {
                    return Err(Error {
                        kind: ErrorKind::InvalidInput,
                        message: Some("No addresses found".to_string()),
                    });
                }
                StdTcpStream::connect(&addrs[..]).map_err(Error::from)?
            } else {
                return Err(Error {
                    kind: ErrorKind::InvalidInput,
                    message: Some("Invalid address".to_string()),
                });
            };

            // Set a read timeout to make reads non-blocking-ish
            stream.set_read_timeout(Some(Duration::from_millis(100))).map_err(Error::from)?;
            stream.set_nonblocking(false).map_err(Error::from)?;

            Ok(Stream { inner: RefCell::new(StreamInner::Plain(stream)) })
        }

        /// Connect with TLS (HTTPS)
        pub fn connect_tls(addr: &str, host: &str) -> Result<Self, Error> {
            let stream = if let Ok(addrs) = addr.to_socket_addrs() {
                let addrs: Vec<_> = addrs.collect();
                if addrs.is_empty() {
                    return Err(Error {
                        kind: ErrorKind::InvalidInput,
                        message: Some("No addresses found".to_string()),
                    });
                }
                StdTcpStream::connect(&addrs[..]).map_err(Error::from)?
            } else {
                return Err(Error {
                    kind: ErrorKind::InvalidInput,
                    message: Some("Invalid address".to_string()),
                });
            };

            // Set a read timeout
            stream.set_read_timeout(Some(Duration::from_millis(100))).map_err(Error::from)?;
            stream.set_nonblocking(false).map_err(Error::from)?;

            // Wrap with TLS
            let connector = TlsConnector::new()?;
            let tls_stream = connector.connect(host, stream)?;

            Ok(Stream { inner: RefCell::new(StreamInner::Tls(tls_stream)) })
        }

        pub fn read(&self, buf: &mut [u8]) -> Result<usize, Error> {
            let mut inner = self.inner.borrow_mut();
            let result = match &mut *inner {
                StreamInner::Plain(stream) => stream.read(buf),
                StreamInner::Tls(stream) => stream.read(buf),
            };
            
            match result {
                Ok(n) => Ok(n),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    Err(Error { kind: ErrorKind::WouldBlock, message: None })
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    Err(Error { kind: ErrorKind::TimedOut, message: None })
                }
                Err(e) => Err(e.into()),
            }
        }

        pub fn write_all(&self, buf: &[u8]) -> Result<(), Error> {
            let mut inner = self.inner.borrow_mut();
            match &mut *inner {
                StreamInner::Plain(stream) => stream.write_all(buf)?,
                StreamInner::Tls(stream) => stream.write_all(buf)?,
            }
            Ok(())
        }
    }

    // Keep TcpStream as an alias for backwards compatibility
    pub type TcpStream = Stream;
}

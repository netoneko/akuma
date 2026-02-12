//! Transport Adapter for TLS
//!
//! Implements `embedded_io::Read` and `embedded_io::Write` traits for
//! `libakuma::net::TcpStream` to make it compatible with embedded-tls blocking mode.

use embedded_io::{ErrorType, Read, Write};
use libakuma::net::{Error as NetError, ErrorKind, TcpStream};

/// Wrapper around TcpStream that implements embedded-io traits
pub struct TcpTransport {
    stream: TcpStream,
    /// Counter for printing progress dots during blocking waits
    wait_counter: u32,
    /// Number of dots printed so far
    dots_printed: u32,
    /// Whether to print dots while waiting
    print_dots: bool,
}

impl TcpTransport {
    /// Create a new transport wrapper around a TcpStream (blocking mode)
    pub fn new(stream: TcpStream) -> Self {
        Self { stream, wait_counter: 0, dots_printed: 0, print_dots: false }
    }

    /// Create a new transport that prints dots while waiting for data
    /// 
    /// This is useful for keeping SSH connections alive during long waits.
    pub fn new_with_dots(stream: TcpStream) -> Self {
        Self { stream, wait_counter: 0, dots_printed: 0, print_dots: true }
    }

    /// Get a reference to the underlying stream
    pub fn inner(&self) -> &TcpStream {
        &self.stream
    }

    /// Consume the wrapper and return the underlying stream
    pub fn into_inner(self) -> TcpStream {
        self.stream
    }

    /// Get the number of dots printed while waiting
    pub fn dots_printed(&self) -> u32 {
        self.dots_printed
    }

    /// Reset the dots counter (call after cleaning up dots)
    pub fn reset_dots(&mut self) {
        self.dots_printed = 0;
    }
}

/// Error type for embedded-io operations
#[derive(Debug)]
pub struct TransportError {
    kind: embedded_io::ErrorKind,
}

impl TransportError {
    fn from_net_error(e: &NetError) -> Self {
        let kind = match e.kind {
            ErrorKind::TimedOut => embedded_io::ErrorKind::TimedOut,
            ErrorKind::ConnectionRefused => embedded_io::ErrorKind::ConnectionRefused,
            ErrorKind::ConnectionReset => embedded_io::ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted => embedded_io::ErrorKind::ConnectionAborted,
            ErrorKind::NotConnected => embedded_io::ErrorKind::NotConnected,
            ErrorKind::BrokenPipe => embedded_io::ErrorKind::BrokenPipe,
            ErrorKind::InvalidInput => embedded_io::ErrorKind::InvalidInput,
            ErrorKind::InvalidData => embedded_io::ErrorKind::InvalidData,
            // WouldBlock and other variants map to Other
            _ => embedded_io::ErrorKind::Other,
        };
        Self { kind }
    }
}

impl embedded_io::Error for TransportError {
    fn kind(&self) -> embedded_io::ErrorKind {
        self.kind
    }
}

impl ErrorType for TcpTransport {
    type Error = TransportError;
}

impl Read for TcpTransport {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        // The kernel recv blocks until data is available (up to 30s timeout),
        // so we just pass through the result. No retry loop needed.
        loop {
            match self.stream.read(buf) {
                Ok(n) => {
                    self.wait_counter = 0;
                    return Ok(n);
                }
                Err(ref e) if e.kind == ErrorKind::WouldBlock || e.kind == ErrorKind::TimedOut => {
                    // Kernel already blocks, so these are rare edge cases.
                    // Retry immediately without sleeping.
                    if self.print_dots {
                        self.wait_counter += 1;
                        if self.wait_counter % 50 == 0 {
                            libakuma::print(".");
                            self.dots_printed += 1;
                        }
                    }
                    continue;
                }
                Err(ref e) => return Err(TransportError::from_net_error(e)),
            }
        }
    }
}

impl Write for TcpTransport {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        loop {
            match self.stream.write(buf) {
                Ok(n) => return Ok(n),
                Err(ref e) if e.kind == ErrorKind::WouldBlock || e.kind == ErrorKind::TimedOut => {
                    // Kernel already blocks, retry immediately without sleeping.
                    continue;
                }
                Err(ref e) => return Err(TransportError::from_net_error(e)),
            }
        }
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        // TCP doesn't have an explicit flush - data is sent immediately
        Ok(())
    }
}

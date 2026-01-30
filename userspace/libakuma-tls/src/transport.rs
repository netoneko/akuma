//! Transport Adapter for TLS
//!
//! Implements `embedded_io::Read` and `embedded_io::Write` traits for
//! `libakuma::net::TcpStream` to make it compatible with embedded-tls blocking mode.

use embedded_io::{ErrorType, Read, Write};
use libakuma::net::{Error as NetError, ErrorKind, TcpStream};

/// Wrapper around TcpStream that implements embedded-io traits
pub struct TcpTransport {
    stream: TcpStream,
}

impl TcpTransport {
    /// Create a new transport wrapper around a TcpStream
    pub fn new(stream: TcpStream) -> Self {
        Self { stream }
    }

    /// Get a reference to the underlying stream
    pub fn inner(&self) -> &TcpStream {
        &self.stream
    }

    /// Consume the wrapper and return the underlying stream
    pub fn into_inner(self) -> TcpStream {
        self.stream
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
        // Loop until we get data or a real error
        // Handle WouldBlock by retrying with a small delay
        loop {
            match self.stream.read(buf) {
                Ok(n) => return Ok(n),
                Err(ref e) if e.kind == ErrorKind::WouldBlock || e.kind == ErrorKind::TimedOut => {
                    // Retry after a short delay
                    libakuma::sleep_ms(10);
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
                Err(ref e) if e.kind == ErrorKind::WouldBlock => {
                    libakuma::sleep_ms(10);
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

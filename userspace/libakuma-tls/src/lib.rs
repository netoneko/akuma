//! Akuma TLS Library
//!
//! Provides TLS 1.3 client connections for userspace programs.
//! Uses embedded-tls in blocking mode with NoVerify (Phase 1).
//!
//! # Example
//!
//! ```no_run
//! use libakuma_tls::https_fetch;
//!
//! let content = https_fetch("https://example.com/file.txt", true).unwrap();
//! ```

#![no_std]

extern crate alloc;

pub mod http;
pub mod rng;
pub mod transport;

use alloc::string::String;

use embedded_tls::blocking::{TlsConfig, TlsConnection, TlsContext};
use embedded_tls::{Aes128GcmSha256, NoVerify, TlsError};

pub use http::{https_fetch, https_get, https_post, HttpHeaders, HttpStream, HttpStreamTls, StreamResult, find_headers_end};
pub use rng::TlsRng;
pub use transport::TcpTransport;

/// TLS read/write buffer sizes (must be >= 16KB for TLS records)
pub const TLS_RECORD_SIZE: usize = 16384;

/// Error type for TLS operations
#[derive(Debug)]
pub enum Error {
    /// DNS resolution failed
    DnsError,
    /// TCP connection failed
    ConnectionError(String),
    /// TLS handshake failed
    TlsError(TlsError),
    /// HTTP protocol error
    HttpError(String),
    /// Invalid URL format
    InvalidUrl,
    /// I/O error during read/write
    IoError,
}

impl From<TlsError> for Error {
    fn from(e: TlsError) -> Self {
        Error::TlsError(e)
    }
}

/// Options for TLS connections
#[derive(Debug, Clone, Copy, Default)]
pub struct TlsOptions {
    /// Skip certificate verification (like curl -k)
    /// Note: In Phase 1, this is always true (NoVerify)
    pub insecure: bool,
}

impl TlsOptions {
    /// Create default options
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable insecure mode (skip certificate verification)
    pub fn insecure(mut self) -> Self {
        self.insecure = true;
        self
    }
}

/// TLS connection wrapper for blocking I/O
///
/// Wraps a TCP transport with TLS encryption using embedded-tls blocking mode.
pub struct TlsStream<'a> {
    conn: TlsConnection<'a, TcpTransport, Aes128GcmSha256>,
}

impl<'a> TlsStream<'a> {
    /// Create and handshake a new TLS connection
    ///
    /// # Arguments
    /// * `transport` - TCP transport wrapper
    /// * `server_name` - Hostname for SNI
    /// * `read_buf` - Buffer for TLS read operations (must be >= 16KB)
    /// * `write_buf` - Buffer for TLS write operations (must be >= 16KB)
    ///
    /// # Returns
    /// A connected TLS stream ready for reading/writing
    pub fn connect(
        transport: TcpTransport,
        server_name: &str,
        read_buf: &'a mut [u8],
        write_buf: &'a mut [u8],
    ) -> Result<Self, Error> {
        // Create TLS config with server name for SNI
        let config = TlsConfig::new().with_server_name(server_name);

        // Create RNG for TLS operations
        let mut rng = TlsRng::new();

        // Create TLS connection wrapper
        let mut conn: TlsConnection<'a, TcpTransport, Aes128GcmSha256> =
            TlsConnection::new(transport, read_buf, write_buf);

        // Create context
        let context = TlsContext::new(&config, &mut rng);

        // Perform TLS handshake with NoVerify (Phase 1)
        // Phase 2 would add proper certificate verification
        conn.open::<TlsRng, NoVerify>(context)?;

        Ok(Self { conn })
    }

    /// Read data from the TLS connection
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        self.conn.read(buf).map_err(|_| Error::IoError)
    }

    /// Write data to the TLS connection
    pub fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
        self.conn.write(buf).map_err(|_| Error::IoError)
    }

    /// Write all data to the TLS connection
    pub fn write_all(&mut self, mut buf: &[u8]) -> Result<(), Error> {
        while !buf.is_empty() {
            let n = self.write(buf)?;
            if n == 0 {
                return Err(Error::IoError);
            }
            buf = &buf[n..];
        }
        Ok(())
    }

    /// Flush the TLS connection
    pub fn flush(&mut self) -> Result<(), Error> {
        self.conn.flush().map_err(|_| Error::IoError)
    }

    /// Close the TLS connection gracefully
    pub fn close(self) -> Result<(), Error> {
        match self.conn.close() {
            Ok(_) => Ok(()),
            Err((_, e)) => Err(Error::TlsError(e)),
        }
    }
}

//! TLS Module
//!
//! Provides TLS 1.3 client connections using embedded-tls.
//! This enables HTTPS support for the curl command.
//!
//! # Certificate Verification
//!
//! Two modes are supported:
//! - Secure mode (default): Uses X509Verifier for certificate validation
//! - Insecure mode (-k flag): Uses NoVerify, skips certificate validation

use embedded_io_async::{Read, Write};
use embedded_tls::{Aes128GcmSha256, NoVerify, TlsConfig, TlsConnection, TlsContext, TlsError};

use crate::tls_rng::TlsRng;
use crate::tls_verifier::X509Verifier;

/// TLS read/write buffer sizes (must be >= 16KB for TLS records)
pub const TLS_RECORD_SIZE: usize = 16384;

// ============================================================================
// TLS Options
// ============================================================================

/// Options for TLS connections
#[derive(Debug, Clone, Copy, Default)]
pub struct TlsOptions {
    /// Skip certificate verification (like curl -k)
    pub insecure: bool,
    /// Enable verbose logging
    pub verbose: bool,
}

impl TlsOptions {
    /// Create default options (secure, non-verbose)
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable insecure mode (skip certificate verification)
    pub fn insecure(mut self) -> Self {
        self.insecure = true;
        self
    }

    /// Enable verbose logging
    pub fn verbose(mut self) -> Self {
        self.verbose = true;
        self
    }
}

// ============================================================================
// TLS Stream
// ============================================================================

/// TLS connection wrapper that implements Read + Write
pub struct TlsStream<'a, T>
where
    T: Read + Write,
{
    conn: TlsConnection<'a, T, Aes128GcmSha256>,
}

impl<'a, T> TlsStream<'a, T>
where
    T: Read + Write,
{
    /// Create and handshake a new TLS connection with default options
    ///
    /// Uses certificate verification by default.
    pub async fn connect(
        transport: T,
        server_name: &str,
        read_buf: &'a mut [u8],
        write_buf: &'a mut [u8],
    ) -> Result<Self, TlsError> {
        Self::connect_with_options(
            transport,
            server_name,
            read_buf,
            write_buf,
            TlsOptions::new(),
        )
        .await
    }

    /// Create and handshake a new TLS connection with custom options
    ///
    /// # Arguments
    /// * `transport` - The underlying TCP socket
    /// * `server_name` - The hostname for SNI and certificate verification
    /// * `read_buf` - Buffer for TLS read operations (must be >= 16KB)
    /// * `write_buf` - Buffer for TLS write operations (must be >= 16KB)
    /// * `options` - TLS connection options
    ///
    /// # Returns
    /// A connected TLS stream ready for reading/writing
    pub async fn connect_with_options(
        transport: T,
        server_name: &str,
        read_buf: &'a mut [u8],
        write_buf: &'a mut [u8],
        options: TlsOptions,
    ) -> Result<Self, TlsError> {
        // Create TLS config with server name for SNI
        let config = TlsConfig::new().with_server_name(server_name);

        // Create RNG for TLS operations
        let mut rng = TlsRng::new();

        // Create TLS connection wrapper
        let mut conn = TlsConnection::new(transport, read_buf, write_buf);

        // Create context
        let context = TlsContext::new(&config, &mut rng);

        // Perform TLS handshake with appropriate verifier
        if options.insecure {
            // Skip certificate verification
            conn.open::<TlsRng, NoVerify>(context).await?;
        } else {
            // Verify server certificate
            conn.open::<TlsRng, X509Verifier<'_, Aes128GcmSha256>>(context)
                .await?;
        }

        Ok(Self { conn })
    }

    /// Close the TLS connection gracefully
    pub async fn close(self) -> Result<(), TlsError> {
        // close() returns Result<T, (T, TlsError)> where T is the transport
        // We just discard the transport and convert to Result<(), TlsError>
        match self.conn.close().await {
            Ok(_transport) => Ok(()),
            Err((_transport, e)) => Err(e),
        }
    }
}

impl<T> embedded_io_async::ErrorType for TlsStream<'_, T>
where
    T: Read + Write,
{
    type Error = TlsError;
}

impl<T> Read for TlsStream<'_, T>
where
    T: Read + Write,
{
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.conn.read(buf).await
    }
}

impl<T> Write for TlsStream<'_, T>
where
    T: Read + Write,
{
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.conn.write(buf).await
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.conn.flush().await
    }
}

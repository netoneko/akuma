//! Akuma TLS Library
//!
//! Provides TLS 1.3 client connections for userspace programs.
//! Uses embedded-tls in blocking mode.
//!
//! # SECURITY: certificate verification is disabled
//!
//! Every TLS connection opened by this crate uses `embedded_tls::NoVerify`.
//! There is no certificate or hostname validation, so the channel is
//! **vulnerable to man-in-the-middle attacks** and must not be trusted over
//! an untrusted network. This is a deliberate size trade-off (no bundled CA
//! root store) tracked by `docs/archive/LIBAKUMA_AUDIT.md` item 3. Do not
//! assume otherwise from the API shape.
//!
//! # Example
//!
//! ```no_run
//! use libakuma_tls::https_fetch;
//!
//! let content = https_fetch("https://example.com/file.txt", None).unwrap(); // None = 20MB default
//! ```

#![no_std]

extern crate alloc;

pub mod http;
pub mod rng;
pub mod transport;

use alloc::string::String;

use embedded_tls::blocking::{TlsConfig, TlsConnection, TlsContext};
use embedded_tls::{Aes128GcmSha256, TlsError, UnsecureProvider};

pub use http::{download_file, download_file_with_headers, https_fetch, https_get, https_get_with_limit, https_post, https_post_with_limit, HttpHeaders, HttpStream, HttpStreamTls, StreamResult, find_headers_end, parse_url, parse_status_line, ParsedUrl};
pub use rng::TlsRng;
pub use transport::TcpTransport;

/// TLS read/write buffer sizes.
/// Must be large enough for the largest TLS 1.3 record on the wire:
/// 5 (header) + 16384 (max plaintext) + 1 (content type) + 16 (AES-GCM tag) = 16406,
/// rounded up with headroom.
pub const TLS_RECORD_SIZE: usize = 17408;

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
    IoError, // Reverted to generic IoError
}

impl From<TlsError> for Error {
    fn from(e: TlsError) -> Self {
        Error::TlsError(e)
    }
}

/// Name a [`TlsError`] variant, and for an aborted handshake the alert that
/// carried it.
///
/// Every caller in the tree used to collapse `Error::TlsError(_)` to the string
/// "TLS handshake failed" — `hget`, `meow` and `scratch` each independently —
/// which is how `api.z.ai` stayed undiagnosed through a whole session
/// (`docs/archive/AMD64_TRASHCAN_ISSUES.md` §5: three TLS stacks, one of them
/// ours, and ours was the only one with nothing to say). The variant is the
/// diagnosis: `InsufficientSpace` means [`TLS_RECORD_SIZE`] is too small,
/// `HandshakeAborted` carries the peer's own alert, and `InvalidCipherSuite`
/// means the server would not take `Aes128GcmSha256` — the one suite this
/// crate is built for. Those three demand completely different fixes and
/// produced exactly the same message.
///
/// `&'static str` and a hand-written match rather than `{:?}`, for the reason
/// `hget` already states at its own call site: this is printed at most once per
/// process and is not worth linking `core::fmt`'s machinery for. The same
/// reason `TLS_RECORD_SIZE` is a constant and not a computation.
pub fn tls_error_name(e: &TlsError) -> &'static str {
    match e {
        TlsError::ConnectionClosed => "peer closed the connection",
        TlsError::Unimplemented => "unimplemented (embedded-tls)",
        TlsError::MissingHandshake => "missing handshake",
        // The two alert-carrying variants are the ones worth unpacking: the
        // description is the server's own stated reason for hanging up.
        TlsError::HandshakeAborted(_, d) => alert_name(AlertSource::Peer, d),
        TlsError::AbortHandshake(_, d) => alert_name(AlertSource::Us, d),
        TlsError::IoError => "I/O error",
        TlsError::InternalError => "internal error",
        TlsError::InvalidRecord => "invalid record",
        TlsError::UnknownContentType => "unknown content type",
        TlsError::InvalidNonceLength => "invalid nonce length",
        TlsError::InvalidTicketLength => "invalid ticket length",
        TlsError::UnknownExtensionType => "unknown extension type",
        // The buffer-size failure. TLS_BUFFER_TRUNCATION_FIX.md is the last
        // time this one was diagnosed, and it took a byte count to do it.
        TlsError::InsufficientSpace => "insufficient buffer space (TLS_RECORD_SIZE too small)",
        TlsError::InvalidHandshake => "invalid handshake",
        TlsError::InvalidCipherSuite => "cipher suite refused (we offer only AES_128_GCM_SHA256)",
        TlsError::InvalidSignatureScheme => "invalid signature scheme",
        TlsError::InvalidSignature => "invalid signature",
        TlsError::InvalidExtensionsLength => "invalid extensions length",
        TlsError::InvalidSessionIdLength => "invalid session id length",
        TlsError::InvalidSupportedVersions => "invalid supported_versions (no TLS 1.3?)",
        TlsError::InvalidApplicationData => "invalid application data",
        TlsError::InvalidKeyShare => "invalid key_share (group mismatch / HelloRetryRequest?)",
        TlsError::InvalidCertificate => "invalid certificate",
        TlsError::InvalidCertificateEntry => "invalid certificate entry",
        TlsError::InvalidCertificateRequest => "invalid certificate request",
        // New in 0.19, and only reachable when a client certificate is
        // configured — which `libakuma-tls` does not do.
        TlsError::InvalidPrivateKey => "invalid private key",
        TlsError::UnableToInitializeCryptoEngine => "cannot initialize crypto engine",
        TlsError::ParseError(_) => "parse error",
        TlsError::OutOfMemory => "out of memory",
        TlsError::CryptoError => "crypto error",
        TlsError::EncodeError => "encode error",
        TlsError::DecodeError => "decode error",
        TlsError::Io(_) => "transport I/O error",
    }
}

/// Which side sent the alert. `HandshakeAborted` is the peer's, `AbortHandshake`
/// is ours — a distinction worth keeping, because "the server rejected us" and
/// "we rejected the server" send you to opposite ends of the code.
enum AlertSource {
    Peer,
    Us,
}

/// The alert description, prefixed by who sent it.
///
/// Returns one `&'static str` per (source, description) pair rather than
/// formatting, so this stays allocation-free and `core::fmt`-free. Only the
/// descriptions a TLS 1.3 handshake can realistically end on are spelled out;
/// the rest fall back to naming the source alone, which still says which side
/// gave up.
fn alert_name(src: AlertSource, d: &embedded_tls::alert::AlertDescription) -> &'static str {
    use embedded_tls::alert::AlertDescription as A;
    match (src, d) {
        (AlertSource::Peer, A::HandshakeFailure) => "handshake aborted by peer: handshake_failure",
        (AlertSource::Peer, A::ProtocolVersion) => "handshake aborted by peer: protocol_version",
        (AlertSource::Peer, A::IllegalParameter) => "handshake aborted by peer: illegal_parameter",
        (AlertSource::Peer, A::InsufficientSecurity) => {
            "handshake aborted by peer: insufficient_security"
        }
        (AlertSource::Peer, A::UnrecognizedName) => {
            "handshake aborted by peer: unrecognized_name (SNI)"
        }
        (AlertSource::Peer, A::MissingExtension) => "handshake aborted by peer: missing_extension",
        (AlertSource::Peer, A::UnsupportedExtension) => {
            "handshake aborted by peer: unsupported_extension"
        }
        (AlertSource::Peer, A::DecodeError) => "handshake aborted by peer: decode_error",
        (AlertSource::Peer, A::InternalError) => "handshake aborted by peer: internal_error",
        (AlertSource::Peer, A::CloseNotify) => "handshake aborted by peer: close_notify",
        (AlertSource::Peer, _) => "handshake aborted by peer",
        (AlertSource::Us, A::DecodeError) => "handshake aborted by us: decode_error",
        (AlertSource::Us, A::IllegalParameter) => "handshake aborted by us: illegal_parameter",
        (AlertSource::Us, A::UnexpectedMessage) => "handshake aborted by us: unexpected_message",
        (AlertSource::Us, A::RecordOverflow) => "handshake aborted by us: record_overflow",
        (AlertSource::Us, A::BadRecordMac) => "handshake aborted by us: bad_record_mac",
        (AlertSource::Us, _) => "handshake aborted by us",
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

        // Perform the handshake WITHOUT certificate verification (Phase 1).
        // Phase 2 would add proper verification.
        //
        // `UnsecureProvider` is how 0.19 spells what used to be the `NoVerify`
        // type argument to `open()`: verification moved into the
        // `CryptoProvider`, so the choice is now made by which provider is
        // handed to `TlsContext` rather than by a type parameter. The security
        // property is unchanged — this still does not verify certificates — and
        // the rename is the whole of the difference.
        //
        // The fork's `pub mod handshake`/`pub mod extensions` exist so a real
        // verifier can be written here without webpki/ring when Phase 2 happens.
        let mut tls_conn = TlsConnection::new(transport, read_buf, write_buf);
        tls_conn.open(TlsContext::new(
            &config,
            UnsecureProvider::new::<Aes128GcmSha256>(TlsRng::new()),
        ))?;

        Ok(Self { conn: tls_conn })
    }

    /// Read data from the TLS connection
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        self.conn.read(buf).map_err(Error::TlsError)
    }

    /// Write data to the TLS connection
    pub fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
        self.conn.write(buf).map_err(Error::TlsError)
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
        self.conn.flush().map_err(Error::TlsError)
    }

    /// Close the TLS connection gracefully
    pub fn close(self) -> Result<(), Error> {
        match self.conn.close() {
            Ok(_) => Ok(()),
            Err((_, e)) => Err(Error::TlsError(e)),
        }
    }
}
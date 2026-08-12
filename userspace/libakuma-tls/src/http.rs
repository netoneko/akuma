//! HTTP/HTTPS Client Helpers
//!
//! Provides functions for HTTP GET/POST requests over HTTP and HTTPS,
//! including streaming response support.
//!
//! All read/stream loops share one code path via the private [`HttpIo`]
//! trait, dispatched dynamically so there is exactly one copy of each loop
//! body in the binary regardless of transport. See
//! `docs/archive/LIBAKUMA_AUDIT.md` item 4.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::net::{resolve, ErrorKind, TcpStream};

use crate::transport::TcpTransport;
use crate::{Error, TlsStream, TLS_RECORD_SIZE};

/// Default maximum response size (20MB) for non-streaming requests
const DEFAULT_MAX_RESPONSE_SIZE: usize = 20 * 1024 * 1024;
/// Maximum buffer size for HTTP headers before considering them malformed (256KB)
const MAX_HEADERS_BUFFER_SIZE: usize = 256 * 1024;
/// Consecutive transient I/O errors tolerated before giving up on a TCP read.
/// The kernel's recv blocks for up to 30s, so genuine WouldBlock is rare; this
/// budget absorbs short hiccups without an unbounded hang.
const TCP_ERROR_BUDGET: u32 = 200;

// ============================================================================
// Shared transport abstraction
// ============================================================================

/// Private read/write abstraction over the two transports (`TcpStream` and
/// `TlsStream`). Methods are written `&mut self` for object safety; helpers
/// below take `&mut dyn HttpIo` so each loop body exists once in the binary.
///
/// Error mapping is transport-specific and intentionally lossy here — a
/// richer `Error` is the audit's item 9, not this refactor.
trait HttpIo {
    /// One read attempt. `Ok(n)` with n>0 = data, `Ok(0)` = EOF, `Err` = failure.
    fn io_read(&mut self, buf: &mut [u8]) -> Result<usize, Error>;
    /// Write all bytes (and flush, for TLS). `Err` only on hard failure.
    fn io_write_all(&mut self, buf: &[u8]) -> Result<(), Error>;
}

impl HttpIo for TlsStream<'_> {
    fn io_read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        self.read(buf)
    }
    fn io_write_all(&mut self, buf: &[u8]) -> Result<(), Error> {
        // TLS needs an explicit flush to push the encrypted record onto the
        // wire; bundle it here so callers don't have to remember per-transport.
        self.write_all(buf)?;
        self.flush()
    }
}

impl HttpIo for TcpStream {
    fn io_read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        TcpStream::read(self, buf).map_err(|_| Error::IoError)
    }
    fn io_write_all(&mut self, buf: &[u8]) -> Result<(), Error> {
        TcpStream::write_all(self, buf).map_err(|_| Error::IoError)
    }
}

// ============================================================================
// HTTP headers
// ============================================================================

/// HTTP headers for requests
pub struct HttpHeaders {
    headers: Vec<(String, String)>,
}

impl HttpHeaders {
    /// Create empty headers
    pub fn new() -> Self {
        Self { headers: Vec::new() }
    }

    /// Add a header
    pub fn add(&mut self, name: &str, value: &str) -> &mut Self {
        self.headers.push((String::from(name), String::from(value)));
        self
    }

    /// Add Authorization: Bearer header
    pub fn bearer_auth(&mut self, token: &str) -> &mut Self {
        self.add("Authorization", &format!("Bearer {}", token))
    }

    /// Add Content-Type header
    pub fn content_type(&mut self, ct: &str) -> &mut Self {
        self.add("Content-Type", ct)
    }

    /// Format headers for HTTP request
    fn format(&self) -> String {
        let mut s = String::new();
        for (name, value) in &self.headers {
            s.push_str(name);
            s.push_str(": ");
            s.push_str(value);
            s.push_str("\r\n");
        }
        s
    }
}

impl Default for HttpHeaders {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// URL parsing + request building
// ============================================================================

/// Parsed URL components
pub struct ParsedUrl<'a> {
    pub is_https: bool,
    pub host: &'a str,
    pub port: u16,
    pub path: &'a str,
}

/// Parse an HTTP(S) URL: `http(s)://host[:port]/path`. `path` defaults to
/// `/` when the URL has none; `port` defaults to 80/443 by scheme.
pub fn parse_url(url: &str) -> Option<ParsedUrl<'_>> {
    let (is_https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return None;
    };

    let default_port = if is_https { 443 } else { 80 };

    let (host_port, path) = match rest.find('/') {
        Some(pos) => (&rest[..pos], &rest[pos..]),
        None => (rest, "/"),
    };

    let (host, port) = match host_port.rfind(':') {
        Some(pos) => {
            let h = &host_port[..pos];
            let p = host_port[pos + 1..].parse::<u16>().ok()?;
            (h, p)
        }
        None => (host_port, default_port),
    };

    Some(ParsedUrl {
        is_https,
        host,
        port,
        path,
    })
}

/// Build an HTTP GET request (no custom headers).
fn build_http_request(host: &str, path: &str) -> String {
    format!(
        "GET {} HTTP/1.0\r\n\
         Host: {}\r\n\
         User-Agent: libakuma-tls/1.0\r\n\
         Connection: close\r\n\
         \r\n",
        path, host
    )
}

/// Build an HTTP GET request with custom headers
fn build_get_request_with_headers(host: &str, path: &str, headers: &HttpHeaders) -> String {
    format!(
        "GET {} HTTP/1.0\r\n\
         Host: {}\r\n\
         User-Agent: libakuma-tls/1.0\r\n\
         {}Connection: close\r\n\
         \r\n",
        path, host, headers.format()
    )
}

/// Build an HTTP POST request with custom headers
fn build_post_request(host: &str, path: &str, body: &str, headers: &HttpHeaders) -> String {
    format!(
        "POST {} HTTP/1.0\r\n\
         Host: {}\r\n\
         User-Agent: libakuma-tls/1.0\r\n\
         {}Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        path, host, headers.format(), body.len(), body
    )
}

/// Resolve + connect the TCP socket for a parsed URL.
fn tcp_connect(parsed: &ParsedUrl) -> Result<TcpStream, Error> {
    let ip = resolve(parsed.host).map_err(|_| Error::DnsError)?;
    let addr_str = format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], parsed.port);
    TcpStream::connect(&addr_str).map_err(|e| Error::ConnectionError(format!("{:?}", e)))
}

// ============================================================================
// Shared response readers
// ============================================================================

/// Parse Content-Length from HTTP headers once we've found the header boundary.
/// Returns (headers_end_offset, content_length_if_present).
fn parse_content_length(data: &[u8]) -> Option<(usize, Option<usize>)> {
    let end = find_headers_end(data)?;
    let header_str = core::str::from_utf8(&data[..end]).ok()?;
    let cl = header_str.lines()
        .find(|line| {
            let bytes = line.as_bytes();
            bytes.len() >= 15 && bytes[..15].eq_ignore_ascii_case(b"content-length:")
        })
        .and_then(|line| line.split(':').nth(1)?.trim().parse::<usize>().ok());
    Some((end, cl))
}

/// Check if the full HTTP response body has been received based on Content-Length.
fn response_complete(data: &[u8], max_size: usize) -> bool {
    if data.len() >= max_size {
        return true;
    }
    if let Some((headers_end, Some(content_length))) = parse_content_length(data) {
        let body_received = data.len() - headers_end;
        body_received >= content_length
    } else {
        false
    }
}

/// Find the end of HTTP headers (\r\n\r\n)
pub fn find_headers_end(data: &[u8]) -> Option<usize> {
    for i in 0..data.len().saturating_sub(3) {
        if &data[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 4);
        }
    }
    None
}

/// Read the response body into a Vec, capped at `max_size`.
///
/// `error_budget` is the number of consecutive transient I/O errors tolerated
/// before breaking: 0 for TLS (errors are fatal — record corruption, etc.),
/// [`TCP_ERROR_BUDGET`] for TCP (WouldBlock after a 30s recv timeout is rare
/// but recoverable). A successful read resets the budget.
///
/// On a hard error before any byte is received, returns `Err(IoError)` so the
/// caller can distinguish "nothing came" from "truncated mid-stream" (the
/// compromise documented in `userspace/libakuma-tls/docs/ERROR_HANDLING_FIX.md`).
fn read_response(s: &mut dyn HttpIo, max_size: usize, error_budget: u32) -> Result<Vec<u8>, Error> {
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    let mut budget = error_budget;
    loop {
        match s.io_read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                budget = error_budget;
                if response.len() + n > max_size {
                    let remaining = max_size - response.len();
                    response.extend_from_slice(&buf[..remaining]);
                    break;
                }
                response.extend_from_slice(&buf[..n]);
                if response_complete(&response, max_size) {
                    break;
                }
            }
            Err(_) => {
                if response.is_empty() {
                    return Err(Error::IoError);
                }
                if budget == 0 {
                    break;
                }
                budget -= 1;
                libakuma::sleep_ms(1);
            }
        }
    }
    Ok(response)
}

/// Read until the end-of-headers marker (`\r\n\r\n`) into `hdr_buf`.
///
/// Returns `Err(HttpError)` if headers exceed [`MAX_HEADERS_BUFFER_SIZE`] or
/// never arrive within [`TCP_ERROR_BUDGET`] consecutive transient errors.
/// Non-I/O errors (e.g. TLS record corruption) propagate immediately.
fn read_until_headers(s: &mut dyn HttpIo, hdr_buf: &mut Vec<u8>) -> Result<(), Error> {
    let mut tmp = [0u8; 16384];
    let mut retries = 0u32;
    loop {
        match s.io_read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                retries = 0;
                hdr_buf.extend_from_slice(&tmp[..n]);
                if find_headers_end(hdr_buf).is_some() {
                    break;
                }
                if hdr_buf.len() > MAX_HEADERS_BUFFER_SIZE {
                    return Err(Error::HttpError(String::from("Headers too large")));
                }
            }
            Err(Error::IoError) => {
                retries += 1;
                if retries >= TCP_ERROR_BUDGET {
                    return Err(Error::IoError);
                }
                libakuma::sleep_ms(1);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Stream the response body to a file descriptor.
///
/// `initial` is body data already buffered from the header read; `content_length`
/// (if known) bounds the total so a server that holds the connection open
/// (HTTP/1.1 keep-alive ignoring our `Connection: close`) doesn't make us
/// block until the recv timeout. Transient I/O errors are tolerated up to
/// [`TCP_ERROR_BUDGET`] times; TLS-record-corruption-class errors stop the
/// stream immediately.
fn stream_body_to_fd(s: &mut dyn HttpIo, fd: i32, initial: &[u8], content_length: Option<usize>) {
    let mut written: usize = 0;
    if !initial.is_empty() {
        libakuma::write_fd(fd, initial);
        written += initial.len();
    }
    if let Some(cl) = content_length {
        if written >= cl {
            return;
        }
    }
    let mut tmp = [0u8; 16384];
    let mut errors = 0u32;
    loop {
        match s.io_read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                errors = 0;
                libakuma::write_fd(fd, &tmp[..n]);
                written += n;
                if let Some(cl) = content_length {
                    if written >= cl {
                        break;
                    }
                }
            }
            Err(Error::IoError) => {
                errors += 1;
                if errors >= TCP_ERROR_BUDGET {
                    break;
                }
                libakuma::sleep_ms(1);
            }
            Err(_) => break,
        }
    }
}

/// Parse HTTP response, extract body
fn parse_http_response(data: &[u8]) -> Result<Vec<u8>, Error> {
    let headers_end = find_headers_end(data)
        .ok_or_else(|| Error::HttpError(String::from("Invalid HTTP response")))?;

    let header_str = core::str::from_utf8(&data[..headers_end])
        .map_err(|_| Error::HttpError(String::from("Invalid HTTP headers")))?;

    if is_chunked_encoding(header_str) {
        return Err(Error::HttpError(String::from("chunked transfer-encoding not supported")));
    }

    let first_line = header_str
        .lines()
        .next()
        .ok_or_else(|| Error::HttpError(String::from("Empty response")))?;

    let mut parts = first_line.split_whitespace();
    let _version = parts
        .next()
        .ok_or_else(|| Error::HttpError(String::from("Missing HTTP version")))?;
    let status: u16 = parts
        .next()
        .ok_or_else(|| Error::HttpError(String::from("Missing status code")))?
        .parse()
        .map_err(|_| Error::HttpError(String::from("Invalid status code")))?;

    if status < 200 || status >= 300 {
        return Err(Error::HttpError(format!("HTTP error: {}", status)));
    }

    Ok(data[headers_end..].to_vec())
}

// ============================================================================
// In-memory fetch API
// ============================================================================

/// Connect, send `request`, read the full response into a Vec, parse it.
/// The transport is chosen by URL scheme; TLS errors are non-retryable, TCP
/// errors get [`TCP_ERROR_BUDGET`] retries — matching the pre-refactor
/// behaviour of `read_response_{tls,tcp}`.
fn fetch_to_vec(parsed: &ParsedUrl, request: &str, max_size: usize) -> Result<Vec<u8>, Error> {
    let mut stream = tcp_connect(parsed)?;
    let response = if parsed.is_https {
        let transport = TcpTransport::new(stream);
        let mut read_buf = alloc::vec![0u8; TLS_RECORD_SIZE];
        let mut write_buf = alloc::vec![0u8; TLS_RECORD_SIZE];
        let mut tls = TlsStream::connect(transport, parsed.host, &mut read_buf, &mut write_buf)?;
        tls.io_write_all(request.as_bytes())?;
        let response = read_response(&mut tls, max_size, 0)?;
        let _ = tls.close();
        response
    } else {
        stream.io_write_all(request.as_bytes())?;
        read_response(&mut stream, max_size, TCP_ERROR_BUDGET)?
    };
    parse_http_response(&response)
}

/// Fetch content from an HTTP or HTTPS URL
///
/// **Certificate verification is disabled** — see the crate-level `// SECURITY:`
/// banner in `lib.rs`. This channel is MITM-able.
///
/// # Arguments
/// * `url` - The URL to fetch (http:// or https://)
/// * `max_size` - Maximum response body size in bytes (None = 20MB default)
///
/// # Returns
/// The response body as a byte vector, or an error
pub fn https_fetch(url: &str, max_size: Option<usize>) -> Result<Vec<u8>, Error> {
    let parsed = parse_url(url).ok_or(Error::InvalidUrl)?;
    let request = build_http_request(parsed.host, parsed.path);
    fetch_to_vec(&parsed, &request, max_size.unwrap_or(DEFAULT_MAX_RESPONSE_SIZE))
}

/// GET content from an HTTP or HTTPS URL with custom headers
///
/// # Arguments
/// * `url` - The URL to fetch (http:// or https://)
/// * `headers` - HTTP headers to include
///
/// # Returns
/// The response body as a byte vector, or an error
///
/// # Example
/// ```no_run
/// use libakuma_tls::http::{https_get, HttpHeaders};
///
/// let mut headers = HttpHeaders::new();
/// headers.bearer_auth("sk-xxx");
///
/// let response = https_get("https://api.openai.com/v1/models", &headers)?;
/// ```
pub fn https_get(url: &str, headers: &HttpHeaders) -> Result<Vec<u8>, Error> {
    https_get_with_limit(url, headers, DEFAULT_MAX_RESPONSE_SIZE)
}

/// GET with explicit max response size
pub fn https_get_with_limit(url: &str, headers: &HttpHeaders, max_size: usize) -> Result<Vec<u8>, Error> {
    let parsed = parse_url(url).ok_or(Error::InvalidUrl)?;
    let request = build_get_request_with_headers(parsed.host, parsed.path, headers);
    fetch_to_vec(&parsed, &request, max_size)
}

/// POST data to an HTTP or HTTPS URL
///
/// # Arguments
/// * `url` - The URL to POST to (http:// or https://)
/// * `body` - The request body
/// * `headers` - Optional HTTP headers
///
/// # Returns
/// The response body as a byte vector, or an error
///
/// # Example
/// ```no_run
/// use libakuma_tls::http::{https_post, HttpHeaders};
///
/// let mut headers = HttpHeaders::new();
/// headers.content_type("application/json");
/// headers.bearer_auth("sk-xxx");
///
/// let body = r#"{"model": "gpt-4", "messages": []}"#;
/// let response = https_post("https://api.openai.com/v1/chat/completions", body, &headers)?;
/// ```
pub fn https_post(url: &str, body: &str, headers: &HttpHeaders) -> Result<Vec<u8>, Error> {
    https_post_with_limit(url, body, headers, DEFAULT_MAX_RESPONSE_SIZE)
}

/// POST with explicit max response size
pub fn https_post_with_limit(url: &str, body: &str, headers: &HttpHeaders, max_size: usize) -> Result<Vec<u8>, Error> {
    let parsed = parse_url(url).ok_or(Error::InvalidUrl)?;
    let request = build_post_request(parsed.host, parsed.path, body, headers);
    fetch_to_vec(&parsed, &request, max_size)
}

// ============================================================================
// Download-to-disk API (with redirect support)
// ============================================================================

/// Resolve a redirect Location header value to an absolute URL.
/// Handles relative paths (/path), protocol-relative (//host/path), and absolute URLs.
fn resolve_redirect_url(location: &str, scheme: &str, host: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        String::from(location)
    } else if location.starts_with("//") {
        format!("{}:{}", scheme, location)
    } else if location.starts_with('/') {
        format!("{}://{}{}", scheme, host, location)
    } else {
        String::from(location)
    }
}

fn extract_location_header(headers: &str) -> Option<String> {
    for line in headers.lines() {
        let bytes = line.as_bytes();
        if bytes.len() >= 9 && bytes[..9].eq_ignore_ascii_case(b"location:") {
            let value = line[9..].trim();
            return Some(String::from(value));
        }
    }
    None
}

/// The status code from an HTTP response's first line (`"HTTP/1.1 200 OK"`),
/// or from a full header block — only the first line is read either way.
pub fn parse_status_line(headers: &str) -> Option<u16> {
    headers
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
}

fn parse_cl_header(headers: &str) -> Option<usize> {
    headers.lines()
        .find(|line| {
            let bytes = line.as_bytes();
            bytes.len() >= 15 && bytes[..15].eq_ignore_ascii_case(b"content-length:")
        })
        .and_then(|line| line.split(':').nth(1)?.trim().parse().ok())
}

/// True if the headers declare `Transfer-Encoding: chunked`. Chunked bodies
/// are not decoded (`docs/archive/LIBAKUMA_AUDIT.md` item 5) — every read
/// path checks this once, right after parsing headers, and refuses rather
/// than handing the caller the raw chunk-size framing as if it were the body.
fn is_chunked_encoding(headers: &str) -> bool {
    headers.lines().any(|line| {
        let bytes = line.as_bytes();
        bytes.len() >= 18
            && bytes[..18].eq_ignore_ascii_case(b"transfer-encoding:")
            && line[18..].to_ascii_lowercase().contains("chunked")
    })
}

/// Send the GET request, read headers, follow up to `max_redirects` 3xx
/// responses, then stream the body to `dest_path`. Used for both
/// [`download_file`] (no redirects) and [`download_file_with_headers`].
fn download_with_redirects(url: &str, dest_path: &str, headers: &HttpHeaders, max_redirects: u8) -> Result<(), Error> {
    let parsed = parse_url(url).ok_or_else(|| {
        libakuma::eprintln(&format!("[dl] InvalidUrl: {:?}", url));
        Error::InvalidUrl
    })?;
    let mut stream = tcp_connect(&parsed)?;
    if parsed.is_https {
        let transport = TcpTransport::new(stream);
        let mut read_buf = alloc::vec![0u8; TLS_RECORD_SIZE];
        let mut write_buf = alloc::vec![0u8; TLS_RECORD_SIZE];
        let mut tls = TlsStream::connect(transport, parsed.host, &mut read_buf, &mut write_buf)?;
        download_impl(&mut tls, "https", parsed.host, parsed.path, dest_path, headers, max_redirects)
    } else {
        download_impl(&mut stream, "http", parsed.host, parsed.path, dest_path, headers, max_redirects)
    }
}

/// Body of [`download_with_redirects`] after the transport is connected.
/// Shared between TCP and TLS via `dyn HttpIo`.
fn download_impl(
    s: &mut dyn HttpIo,
    scheme: &str,
    host: &str,
    path: &str,
    dest_path: &str,
    headers: &HttpHeaders,
    max_redirects: u8,
) -> Result<(), Error> {
    let request = build_get_request_with_headers(host, path, headers);
    s.io_write_all(request.as_bytes())?;

    let mut hdr_buf = Vec::new();
    read_until_headers(s, &mut hdr_buf)?;

    let end = find_headers_end(&hdr_buf)
        .ok_or_else(|| Error::HttpError(String::from("No headers in response")))?;
    let header_str = core::str::from_utf8(&hdr_buf[..end])
        .map_err(|_| Error::HttpError(String::from("Invalid headers")))?;
    let status = parse_status_line(header_str).unwrap_or(0);

    if status >= 300 && status < 400 && max_redirects > 0 {
        if let Some(location) = extract_location_header(header_str) {
            let absolute = resolve_redirect_url(&location, scheme, host);
            // Auth headers stripped on redirect (typical safe default — the
            // target usually has its own creds in the URL or via cookies).
            return download_with_redirects(&absolute, dest_path, &HttpHeaders::new(), max_redirects - 1);
        }
    }

    if status < 200 || status >= 300 {
        return Err(Error::HttpError(format!("HTTP error: {}", status)));
    }

    if is_chunked_encoding(header_str) {
        return Err(Error::HttpError(String::from("chunked transfer-encoding not supported")));
    }

    let content_length = parse_cl_header(header_str);
    let fd = libakuma::open(
        dest_path,
        libakuma::open_flags::O_WRONLY | libakuma::open_flags::O_CREAT | libakuma::open_flags::O_TRUNC,
    );
    if fd < 0 {
        return Err(Error::IoError);
    }

    stream_body_to_fd(s, fd, &hdr_buf[end..], content_length);
    libakuma::close(fd);
    Ok(())
}

/// Download a file from an HTTP or HTTPS URL and save it to disk.
///
/// This function streams the response body directly to a file, which is
/// memory-efficient for large files. Does not follow redirects — use
/// [`download_file_with_headers`] for that.
///
/// # Arguments
/// * `url` - The URL to fetch (http:// or https://)
/// * `dest_path` - The path on the local filesystem to save the file to.
///
/// # Returns
/// Ok(()) on success, or an error.
pub fn download_file(url: &str, dest_path: &str) -> Result<(), Error> {
    download_with_redirects(url, dest_path, &HttpHeaders::new(), 0)
}

/// Download a file from an HTTP/HTTPS URL with custom headers and redirect support.
///
/// Follows up to 5 HTTP 3xx redirects. Auth headers are stripped on redirect
/// (the redirect target typically has its own auth via query parameters).
pub fn download_file_with_headers(url: &str, dest_path: &str, headers: &HttpHeaders) -> Result<(), Error> {
    download_with_redirects(url, dest_path, headers, 5)
}

// ============================================================================
// Streaming HTTP Client
// ============================================================================

/// Streaming HTTP response reader
///
/// Provides a unified interface for reading streaming HTTP responses
/// from both HTTP and HTTPS connections.
pub struct HttpStream {
    conn: ConnectionState,
    pending_data: Vec<u8>,
    headers_parsed: bool,
    status_code: u16,
}

enum ConnectionState {
    Tcp(TcpStream),
}

/// Result of a streaming read operation
pub enum StreamResult {
    /// Data was read successfully
    Data(Vec<u8>),
    /// No data available yet (would block)
    WouldBlock,
    /// Connection closed / end of response
    Done,
    /// An error occurred
    Error(Error),
}

/// Shared header/body demultiplexing used by both `HttpStream` and
/// `HttpStreamTls`. Returns `WouldBlock` until the header terminator is seen,
/// then hands each subsequent chunk through as `Data`.
fn process_pending(pending: &mut Vec<u8>, headers_parsed: &mut bool, status_code: &mut u16) -> StreamResult {
    if !*headers_parsed {
        if let Some(pos) = find_headers_end(pending) {
            let header_str = core::str::from_utf8(&pending[..pos]).unwrap_or("");
            *status_code = parse_status_line(header_str).unwrap_or(0);
            let chunked = is_chunked_encoding(header_str);
            *headers_parsed = true;
            pending.drain(..pos);
            if *status_code < 200 || *status_code >= 300 {
                return StreamResult::Error(Error::HttpError(
                    format!("HTTP error: {}", *status_code)
                ));
            }
            if chunked {
                return StreamResult::Error(Error::HttpError(
                    String::from("chunked transfer-encoding not supported")
                ));
            }
        }
        return StreamResult::WouldBlock;
    }

    if pending.is_empty() {
        StreamResult::WouldBlock
    } else {
        let data = core::mem::take(pending);
        StreamResult::Data(data)
    }
}

impl HttpStream {
    /// Create a new streaming HTTP connection (HTTP only)
    ///
    /// For HTTPS, use `HttpStreamTls` instead.
    ///
    /// # Arguments
    /// * `url` - Base URL (e.g., "http://10.0.2.2:11434")
    ///
    /// # Returns
    /// A new HttpStream ready to send requests
    pub fn connect(url: &str) -> Result<Self, Error> {
        let parsed = parse_url(url).ok_or(Error::InvalidUrl)?;

        if parsed.is_https {
            return Err(Error::HttpError(String::from("Use HttpStreamTls for HTTPS")));
        }

        let stream = tcp_connect(&parsed)?;

        Ok(Self {
            conn: ConnectionState::Tcp(stream),
            pending_data: Vec::new(),
            headers_parsed: false,
            status_code: 0,
        })
    }

    /// Send a POST request (for streaming response)
    ///
    /// After calling this, use `read_chunk()` to read the streaming response.
    pub fn post(&mut self, host: &str, path: &str, body: &str, headers: &HttpHeaders) -> Result<(), Error> {
        match &mut self.conn {
            ConnectionState::Tcp(stream) => {
                let request = format!(
                    "POST {} HTTP/1.0\r\n\
                     Host: {}\r\n\
                     {}Content-Length: {}\r\n\
                     Connection: close\r\n\
                     \r\n\
                     {}",
                    path, host, headers.format(), body.len(), body
                );
                stream.write_all(request.as_bytes())
                    .map_err(|_| Error::IoError)?;
                Ok(())
            }
        }
    }

    /// Read the next chunk of streaming data
    ///
    /// Returns StreamResult indicating what happened
    pub fn read_chunk(&mut self) -> StreamResult {
        let mut buf = [0u8; 4096];

        let read_result = match &self.conn {
            ConnectionState::Tcp(stream) => stream.read(&mut buf),
        };

        match read_result {
            Ok(0) => StreamResult::Done,
            Ok(n) => {
                self.pending_data.extend_from_slice(&buf[..n]);
                process_pending(&mut self.pending_data, &mut self.headers_parsed, &mut self.status_code)
            }
            Err(ref e) if e.kind == ErrorKind::WouldBlock || e.kind == ErrorKind::TimedOut => {
                StreamResult::WouldBlock
            }
            Err(_) => StreamResult::Error(Error::IoError),
        }
    }

    /// Get the HTTP status code (available after headers are parsed)
    pub fn status_code(&self) -> u16 {
        self.status_code
    }

    /// Check if headers have been parsed
    pub fn headers_parsed(&self) -> bool {
        self.headers_parsed
    }
}

/// Streaming HTTP client for HTTPS connections
///
/// This is a separate type because TLS requires owning the buffers.
pub struct HttpStreamTls<'a> {
    tls: TlsStream<'a>,
    pending_data: Vec<u8>,
    headers_parsed: bool,
    status_code: u16,
}

impl<'a> HttpStreamTls<'a> {
    /// Create a new HTTPS streaming connection
    ///
    /// # Arguments
    /// * `stream` - TCP stream to wrap
    /// * `host` - Hostname for SNI
    /// * `read_buf` - TLS read buffer (must be >= TLS_RECORD_SIZE)
    /// * `write_buf` - TLS write buffer (must be >= TLS_RECORD_SIZE)
    pub fn connect(
        stream: TcpStream,
        host: &str,
        read_buf: &'a mut [u8],
        write_buf: &'a mut [u8],
    ) -> Result<Self, Error> {
        let transport = TcpTransport::new(stream);
        let tls = TlsStream::connect(transport, host, read_buf, write_buf)?;

        Ok(Self {
            tls,
            pending_data: Vec::new(),
            headers_parsed: false,
            status_code: 0,
        })
    }

    /// Send a POST request
    pub fn post(&mut self, host: &str, path: &str, body: &str, headers: &HttpHeaders) -> Result<(), Error> {
        let request = format!(
            "POST {} HTTP/1.0\r\n\
             Host: {}\r\n\
             {}Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n\
             {}",
            path, host, headers.format(), body.len(), body
        );

        self.tls.write_all(request.as_bytes())?;
        self.tls.flush()?;
        Ok(())
    }

    /// POST with the body streamed from an already-open file descriptor.
    ///
    /// `body_fd` must be positioned at the start of the body and `content_length`
    /// must equal the number of body bytes available from it. The body is read
    /// and written in fixed-size chunks so the full request is never held in
    /// memory at once — the caller only ever needs the file on disk.
    pub fn post_from_fd(
        &mut self,
        host: &str,
        path: &str,
        content_length: usize,
        body_fd: i32,
        headers: &HttpHeaders,
    ) -> Result<(), Error> {
        let header = format!(
            "POST {} HTTP/1.0\r\n\
             Host: {}\r\n\
             {}Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n",
            path, host, headers.format(), content_length
        );
        self.tls.write_all(header.as_bytes())?;

        let mut buf = [0u8; 8192];
        loop {
            let n = libakuma::read_fd(body_fd, &mut buf);
            if n <= 0 {
                break;
            }
            self.tls.write_all(&buf[..n as usize])?;
        }
        self.tls.flush()?;
        Ok(())
    }

    /// Read the next chunk of data
    pub fn read_chunk(&mut self) -> StreamResult {
        let mut buf = [0u8; 4096];

        match self.tls.read(&mut buf) {
            Ok(0) => StreamResult::Done,
            Ok(n) => {
                self.pending_data.extend_from_slice(&buf[..n]);
                process_pending(&mut self.pending_data, &mut self.headers_parsed, &mut self.status_code)
            }
            Err(e) => StreamResult::Error(e),
        }
    }

    /// Get the HTTP status code (available after headers are parsed)
    pub fn status_code(&self) -> u16 {
        self.status_code
    }

    /// Check if headers have been parsed
    pub fn headers_parsed(&self) -> bool {
        self.headers_parsed
    }
}

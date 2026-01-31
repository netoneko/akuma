//! HTTP/HTTPS Client Helpers
//!
//! Provides functions for HTTP GET/POST requests over HTTP and HTTPS,
//! including streaming response support.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::net::{resolve, ErrorKind, TcpStream};

use crate::transport::TcpTransport;
use crate::{Error, TlsStream, TLS_RECORD_SIZE};

/// Maximum response size (64KB) for non-streaming requests
const MAX_RESPONSE_SIZE: usize = 64 * 1024;

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

/// Fetch content from an HTTP or HTTPS URL
///
/// # Arguments
/// * `url` - The URL to fetch (http:// or https://)
/// * `insecure` - If true, skip TLS certificate verification (like curl -k)
///
/// # Returns
/// The response body as a byte vector, or an error
///
/// # Example
/// ```no_run
/// let content = https_fetch("https://raw.githubusercontent.com/user/repo/main/file.txt", true)?;
/// ```
pub fn https_fetch(url: &str, _insecure: bool) -> Result<Vec<u8>, Error> {
    let parsed = parse_url(url).ok_or(Error::InvalidUrl)?;

    // Resolve hostname
    let ip = resolve(parsed.host).map_err(|_| Error::DnsError)?;
    let addr_str = format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], parsed.port);

    // Connect TCP
    let stream = TcpStream::connect(&addr_str)
        .map_err(|e| Error::ConnectionError(format!("{:?}", e)))?;

    if parsed.is_https {
        // HTTPS - wrap in TLS
        let transport = TcpTransport::new(stream);

        // Allocate TLS buffers
        let mut read_buf = alloc::vec![0u8; TLS_RECORD_SIZE];
        let mut write_buf = alloc::vec![0u8; TLS_RECORD_SIZE];

        let mut tls = TlsStream::connect(
            transport,
            parsed.host,
            &mut read_buf,
            &mut write_buf,
        )?;

        // Send HTTP request
        let request = build_http_request(parsed.host, parsed.path);
        tls.write_all(request.as_bytes())?;
        tls.flush()?;

        // Read response
        let response = read_response_tls(&mut tls)?;

        // Close TLS gracefully (ignore errors on close)
        let _ = tls.close();

        // Parse HTTP response
        parse_http_response(&response)
    } else {
        // Plain HTTP
        let request = build_http_request(parsed.host, parsed.path);
        stream.write_all(request.as_bytes())
            .map_err(|_| Error::IoError)?;

        // Read response
        let response = read_response_tcp(&stream)?;

        // Parse HTTP response
        parse_http_response(&response)
    }
}

/// Parsed URL components
struct ParsedUrl<'a> {
    is_https: bool,
    host: &'a str,
    port: u16,
    path: &'a str,
}

/// Parse an HTTP(S) URL
fn parse_url(url: &str) -> Option<ParsedUrl<'_>> {
    let (is_https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return None;
    };

    let default_port = if is_https { 443 } else { 80 };

    // Split host:port from path
    let (host_port, path) = match rest.find('/') {
        Some(pos) => (&rest[..pos], &rest[pos..]),
        None => (rest, "/"),
    };

    // Parse host and port
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

/// Build an HTTP GET request
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
    let parsed = parse_url(url).ok_or(Error::InvalidUrl)?;

    // Resolve hostname
    let ip = resolve(parsed.host).map_err(|_| Error::DnsError)?;
    let addr_str = format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], parsed.port);

    // Connect TCP
    let stream = TcpStream::connect(&addr_str)
        .map_err(|e| Error::ConnectionError(format!("{:?}", e)))?;

    if parsed.is_https {
        // HTTPS - wrap in TLS
        let transport = TcpTransport::new(stream);

        // Allocate TLS buffers
        let mut read_buf = alloc::vec![0u8; TLS_RECORD_SIZE];
        let mut write_buf = alloc::vec![0u8; TLS_RECORD_SIZE];

        let mut tls = TlsStream::connect(
            transport,
            parsed.host,
            &mut read_buf,
            &mut write_buf,
        )?;

        // Send HTTP request
        let request = build_get_request_with_headers(parsed.host, parsed.path, headers);
        tls.write_all(request.as_bytes())?;
        tls.flush()?;

        // Read response
        let response = read_response_tls(&mut tls)?;

        // Close TLS gracefully (ignore errors on close)
        let _ = tls.close();

        // Parse HTTP response
        parse_http_response(&response)
    } else {
        // Plain HTTP
        let request = build_get_request_with_headers(parsed.host, parsed.path, headers);
        stream.write_all(request.as_bytes())
            .map_err(|_| Error::IoError)?;

        // Read response
        let response = read_response_tcp(&stream)?;

        // Parse HTTP response
        parse_http_response(&response)
    }
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
    let parsed = parse_url(url).ok_or(Error::InvalidUrl)?;

    // Resolve hostname
    let ip = resolve(parsed.host).map_err(|_| Error::DnsError)?;
    let addr_str = format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], parsed.port);

    // Connect TCP
    let stream = TcpStream::connect(&addr_str)
        .map_err(|e| Error::ConnectionError(format!("{:?}", e)))?;

    if parsed.is_https {
        // HTTPS - wrap in TLS
        let transport = TcpTransport::new(stream);

        // Allocate TLS buffers
        let mut read_buf = alloc::vec![0u8; TLS_RECORD_SIZE];
        let mut write_buf = alloc::vec![0u8; TLS_RECORD_SIZE];

        let mut tls = TlsStream::connect(
            transport,
            parsed.host,
            &mut read_buf,
            &mut write_buf,
        )?;

        // Send HTTP request
        let request = build_post_request(parsed.host, parsed.path, body, headers);
        tls.write_all(request.as_bytes())?;
        tls.flush()?;

        // Read response
        let response = read_response_tls(&mut tls)?;

        // Close TLS gracefully (ignore errors on close)
        let _ = tls.close();

        // Parse HTTP response
        parse_http_response(&response)
    } else {
        // Plain HTTP
        let request = build_post_request(parsed.host, parsed.path, body, headers);
        stream.write_all(request.as_bytes())
            .map_err(|_| Error::IoError)?;

        // Read response
        let response = read_response_tcp(&stream)?;

        // Parse HTTP response
        parse_http_response(&response)
    }
}

/// Read HTTP response from TLS stream
fn read_response_tls(tls: &mut TlsStream<'_>) -> Result<Vec<u8>, Error> {
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];

    loop {
        match tls.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => {
                if response.len() + n > MAX_RESPONSE_SIZE {
                    // Truncate to max size
                    let remaining = MAX_RESPONSE_SIZE - response.len();
                    response.extend_from_slice(&buf[..remaining]);
                    break;
                }
                response.extend_from_slice(&buf[..n]);
            }
            Err(_) => break, // Error or connection closed
        }
    }

    Ok(response)
}

/// Read HTTP response from TCP stream
fn read_response_tcp(stream: &TcpStream) -> Result<Vec<u8>, Error> {
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    let mut empty_reads = 0u32;

    loop {
        match stream.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => {
                empty_reads = 0;
                if response.len() + n > MAX_RESPONSE_SIZE {
                    let remaining = MAX_RESPONSE_SIZE - response.len();
                    response.extend_from_slice(&buf[..remaining]);
                    break;
                }
                response.extend_from_slice(&buf[..n]);
            }
            Err(ref e)
                if e.kind == libakuma::net::ErrorKind::WouldBlock
                    || e.kind == libakuma::net::ErrorKind::TimedOut =>
            {
                empty_reads += 1;
                if empty_reads > 500 {
                    // Timeout after ~5 seconds of no data
                    break;
                }
                libakuma::sleep_ms(10);
                continue;
            }
            Err(_) => break,
        }
    }

    Ok(response)
}

/// Parse HTTP response, extract body
fn parse_http_response(data: &[u8]) -> Result<Vec<u8>, Error> {
    // Find headers end
    let headers_end = find_headers_end(data)
        .ok_or_else(|| Error::HttpError(String::from("Invalid HTTP response")))?;

    // Parse status line
    let header_str = core::str::from_utf8(&data[..headers_end])
        .map_err(|_| Error::HttpError(String::from("Invalid HTTP headers")))?;

    let first_line = header_str
        .lines()
        .next()
        .ok_or_else(|| Error::HttpError(String::from("Empty response")))?;

    // Parse "HTTP/1.x STATUS MESSAGE"
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

    // Return body
    Ok(data[headers_end..].to_vec())
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

        // Resolve hostname
        let ip = resolve(parsed.host).map_err(|_| Error::DnsError)?;
        let addr_str = format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], parsed.port);

        // Connect TCP
        let stream = TcpStream::connect(&addr_str)
            .map_err(|e| Error::ConnectionError(format!("{:?}", e)))?;

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
    pub fn post(&mut self, path: &str, body: &str, headers: &HttpHeaders) -> Result<(), Error> {
        match &mut self.conn {
            ConnectionState::Tcp(stream) => {
                let request = format!(
                    "POST {} HTTP/1.0\r\n\
                     {}Content-Length: {}\r\n\
                     Connection: close\r\n\
                     \r\n\
                     {}",
                    path, headers.format(), body.len(), body
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
                self.process_pending_data()
            }
            Err(ref e) if e.kind == ErrorKind::WouldBlock || e.kind == ErrorKind::TimedOut => {
                StreamResult::WouldBlock
            }
            Err(_) => StreamResult::Done,
        }
    }

    fn process_pending_data(&mut self) -> StreamResult {
        if !self.headers_parsed {
            if let Some(pos) = find_headers_end(&self.pending_data) {
                // Parse headers
                let header_str = core::str::from_utf8(&self.pending_data[..pos]).unwrap_or("");

                // Extract status code
                if let Some(status_line) = header_str.lines().next() {
                    if let Some(code_str) = status_line.split_whitespace().nth(1) {
                        self.status_code = code_str.parse().unwrap_or(0);
                    }
                }

                self.headers_parsed = true;
                self.pending_data.drain(..pos);

                if self.status_code < 200 || self.status_code >= 300 {
                    return StreamResult::Error(Error::HttpError(
                        format!("HTTP error: {}", self.status_code)
                    ));
                }
            }
            // Not enough data for headers yet
            return StreamResult::WouldBlock;
        }

        // Return any pending body data
        if self.pending_data.is_empty() {
            StreamResult::WouldBlock
        } else {
            let data = core::mem::take(&mut self.pending_data);
            StreamResult::Data(data)
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
        // Use dot-printing transport to keep SSH connections alive
        let transport = TcpTransport::new_with_dots(stream);
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

    /// Read the next chunk of data
    pub fn read_chunk(&mut self) -> StreamResult {
        let mut buf = [0u8; 4096];

        match self.tls.read(&mut buf) {
            Ok(0) => StreamResult::Done,
            Ok(n) => {
                self.pending_data.extend_from_slice(&buf[..n]);
                self.process_pending_data()
            }
            Err(_) => StreamResult::Done,
        }
    }

    fn process_pending_data(&mut self) -> StreamResult {
        if !self.headers_parsed {
            if let Some(pos) = find_headers_end(&self.pending_data) {
                let header_str = core::str::from_utf8(&self.pending_data[..pos]).unwrap_or("");

                if let Some(status_line) = header_str.lines().next() {
                    if let Some(code_str) = status_line.split_whitespace().nth(1) {
                        self.status_code = code_str.parse().unwrap_or(0);
                    }
                }

                self.headers_parsed = true;
                self.pending_data.drain(..pos);

                if self.status_code < 200 || self.status_code >= 300 {
                    return StreamResult::Error(Error::HttpError(
                        format!("HTTP error: {}", self.status_code)
                    ));
                }
            }
            return StreamResult::WouldBlock;
        }

        if self.pending_data.is_empty() {
            StreamResult::WouldBlock
        } else {
            let data = core::mem::take(&mut self.pending_data);
            StreamResult::Data(data)
        }
    }

    /// Get the HTTP status code
    pub fn status_code(&self) -> u16 {
        self.status_code
    }

    /// Check if headers have been parsed
    pub fn headers_parsed(&self) -> bool {
        self.headers_parsed
    }
}

//! HTTPS Fetch Helper
//!
//! Provides a simple function to fetch content from HTTP and HTTPS URLs.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::net::{resolve, TcpStream};

use crate::transport::TcpTransport;
use crate::{Error, TlsStream, TLS_RECORD_SIZE};

/// Maximum response size (64KB)
const MAX_RESPONSE_SIZE: usize = 64 * 1024;

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
fn find_headers_end(data: &[u8]) -> Option<usize> {
    for i in 0..data.len().saturating_sub(3) {
        if &data[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 4);
        }
    }
    None
}

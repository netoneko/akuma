//! HTTP client helpers for Git protocol
//!
//! Provides HTTP GET and POST for communicating with Git servers.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::net::{resolve, TcpStream};
use libakuma::print;
use libakuma_tls::transport::TcpTransport;
use libakuma_tls::{find_headers_end, TlsStream, TLS_RECORD_SIZE};

use crate::error::{Error, Result};

/// Parsed URL
#[derive(Debug, Clone)]
pub struct Url {
    pub https: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    /// Parse a Git URL
    ///
    /// Supports:
    /// - https://github.com/owner/repo
    /// - https://github.com/owner/repo.git
    /// - http://host:port/path
    pub fn parse(url: &str) -> Result<Self> {
        let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else {
            return Err(Error::invalid_url());
        };

        let default_port = if https { 443 } else { 80 };

        // Split host from path
        let (host_port, path) = match rest.find('/') {
            Some(pos) => (&rest[..pos], &rest[pos..]),
            None => (rest, "/"),
        };

        // Parse host and port
        let (host, port) = match host_port.rfind(':') {
            Some(pos) => {
                let h = &host_port[..pos];
                let p = host_port[pos + 1..].parse::<u16>()
                    .map_err(|_| Error::invalid_url())?;
                (h, p)
            }
            None => (host_port, default_port),
        };

        // Normalize path (add .git if needed for GitHub)
        let path = if !path.ends_with(".git") && !path.ends_with("/") {
            format!("{}.git", path)
        } else {
            String::from(path)
        };

        Ok(Url {
            https,
            host: String::from(host),
            port,
            path,
        })
    }

    /// Get the URL for info/refs
    pub fn info_refs_url(&self) -> String {
        format!("{}/info/refs?service=git-upload-pack", self.path)
    }

    /// Get the URL for git-upload-pack
    pub fn upload_pack_url(&self) -> String {
        format!("{}/git-upload-pack", self.path)
    }

    /// Get the URL for info/refs for receive-pack (push)
    pub fn info_refs_receive_url(&self) -> String {
        format!("{}/info/refs?service=git-receive-pack", self.path)
    }

    /// Get the URL for git-receive-pack (push)
    pub fn receive_pack_url(&self) -> String {
        format!("{}/git-receive-pack", self.path)
    }
}

/// HTTP response
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// Get a header value (case-insensitive)
    pub fn header(&self, name: &str) -> Option<&str> {
        let name_lower = name.to_lowercase();
        for (k, v) in &self.headers {
            if k.to_lowercase() == name_lower {
                return Some(v);
            }
        }
        None
    }
}

/// HTTP client
pub struct HttpClient {
    url: Url,
    /// Cached resolved IP address (to avoid repeated DNS lookups)
    resolved_ip: Option<[u8; 4]>,
}

impl HttpClient {
    pub fn new(url: Url) -> Self {
        Self { url, resolved_ip: None }
    }
    
    /// Resolve and cache the IP address
    pub fn get_ip(&mut self) -> Result<[u8; 4]> {
        if let Some(ip) = self.resolved_ip {
            return Ok(ip);
        }
        
        let ip = resolve(&self.url.host)
            .map_err(|_| Error::network("DNS resolution failed"))?;
        
        self.resolved_ip = Some(ip);
        Ok(ip)
    }

    /// Send GET request
    pub fn get(&mut self, path: &str) -> Result<Response> {
        let request = format!(
            "GET {} HTTP/1.1\r\n\
             Host: {}\r\n\
             User-Agent: scratch/1.0\r\n\
             Accept: */*\r\n\
             Connection: close\r\n\
             \r\n",
            path, self.url.host
        );

        self.send_request(&request)
    }

    /// Send POST request with body
    pub fn post(&mut self, path: &str, content_type: &str, body: &[u8]) -> Result<Response> {
        self.post_with_auth(path, content_type, body, None)
    }

    /// Send POST request with optional authentication
    pub fn post_with_auth(
        &mut self,
        path: &str,
        content_type: &str,
        body: &[u8],
        auth: Option<&str>,
    ) -> Result<Response> {
        let auth_header = match auth {
            Some(a) => format!("Authorization: {}\r\n", a),
            None => String::new(),
        };

        let request = format!(
            "POST {} HTTP/1.1\r\n\
             Host: {}\r\n\
             User-Agent: scratch/1.0\r\n\
             Content-Type: {}\r\n\
             Content-Length: {}\r\n\
             {}Accept: */*\r\n\
             Connection: close\r\n\
             \r\n",
            path, self.url.host, content_type, body.len(), auth_header
        );

        self.send_request_with_body(&request, body)
    }

    /// Send GET request with optional authentication
    pub fn get_with_auth(&mut self, path: &str, auth: Option<&str>) -> Result<Response> {
        let auth_header = match auth {
            Some(a) => format!("Authorization: {}\r\n", a),
            None => String::new(),
        };

        let request = format!(
            "GET {} HTTP/1.1\r\n\
             Host: {}\r\n\
             User-Agent: scratch/1.0\r\n\
             {}Accept: */*\r\n\
             Connection: close\r\n\
             \r\n",
            path, self.url.host, auth_header
        );

        self.send_request(&request)
    }

    fn send_request(&mut self, request: &str) -> Result<Response> {
        self.send_request_with_body(request, &[])
    }

    fn send_request_with_body(&mut self, request: &str, body: &[u8]) -> Result<Response> {
        // Resolve host (uses cache)
        let ip = self.get_ip()?;

        let addr = format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], self.url.port);

        // Connect
        let stream = TcpStream::connect(&addr)
            .map_err(|_| Error::network("connection failed"))?;

        if self.url.https {
            self.send_https(stream, request, body)
        } else {
            self.send_http(stream, request, body)
        }
    }

    fn send_http(&self, stream: TcpStream, request: &str, body: &[u8]) -> Result<Response> {
        // Send request
        stream.write_all(request.as_bytes())
            .map_err(|_| Error::network("failed to send request"))?;
        
        if !body.is_empty() {
            stream.write_all(body)
                .map_err(|_| Error::network("failed to send body"))?;
        }

        // Read response
        let response = read_http_response(&stream)?;
        parse_response(&response)
    }

    fn send_https(&self, stream: TcpStream, request: &str, body: &[u8]) -> Result<Response> {
        let transport = TcpTransport::new(stream);

        // Allocate TLS buffers
        let mut read_buf = alloc::vec![0u8; TLS_RECORD_SIZE];
        let mut write_buf = alloc::vec![0u8; TLS_RECORD_SIZE];

        let mut tls = TlsStream::connect(
            transport,
            &self.url.host,
            &mut read_buf,
            &mut write_buf,
        ).map_err(|_| Error::network("TLS handshake failed"))?;

        // Send request
        tls.write_all(request.as_bytes())
            .map_err(|_| Error::network("failed to send request"))?;
        
        if !body.is_empty() {
            tls.write_all(body)
                .map_err(|_| Error::network("failed to send body"))?;
        }
        
        tls.flush().map_err(|_| Error::network("failed to flush"))?;

        // Read response
        let response = read_tls_response(&mut tls)?;
        
        let _ = tls.close();
        
        parse_response(&response)
    }
}

fn read_http_response(stream: &TcpStream) -> Result<Vec<u8>> {
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    let mut empty_reads = 0u32;

    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                empty_reads = 0;
                response.extend_from_slice(&buf[..n]);
            }
            Err(ref e) if e.kind == libakuma::net::ErrorKind::WouldBlock ||
                          e.kind == libakuma::net::ErrorKind::TimedOut => {
                empty_reads += 1;
                if empty_reads > 5000 {
                    break;
                }
                libakuma::sleep_ms(1);
            }
            Err(_) => break,
        }
    }

    Ok(response)
}

fn read_tls_response(tls: &mut TlsStream<'_>) -> Result<Vec<u8>> {
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    let mut last_report = 0usize;
    let mut last_report_time = libakuma::uptime();

    loop {
        match tls.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                response.extend_from_slice(&buf[..n]);
                // Print progress every 64KB
                if response.len() - last_report >= 65536 {
                    let now = libakuma::uptime();
                    let interval_bytes = response.len() - last_report;
                    let interval_time = now - last_report_time;
                    print("scratch: received ");
                    print_size_kb(response.len());
                    print(" (");
                    print_speed_kbps(interval_bytes, interval_time);
                    print(")    \r");
                    last_report = response.len();
                    last_report_time = now;
                }
            }
            Err(_) => break,
        }
    }

    if response.len() > 65536 {
        print("\n");
    }

    Ok(response)
}

fn print_size_kb(bytes: usize) {
    let kb = bytes / 1024;
    print_num(kb);
    print(" KB");
}

fn print_speed_kbps(bytes: usize, elapsed_us: u64) {
    if elapsed_us == 0 {
        print("-- kbps");
        return;
    }
    // kbps = (bytes / 1024) / (elapsed_us / 1_000_000) = bytes * 1_000_000 / (1024 * elapsed_us)
    // Simplify to avoid overflow: bytes * 976 / elapsed_us (approx 1000000/1024)
    let kbps = (bytes as u64 * 976) / elapsed_us;
    print_num(kbps as usize);
    print(" kbps");
}

fn print_num(n: usize) {
    if n == 0 {
        print("0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 0;
    let mut val = n;
    while val > 0 {
        buf[i] = b'0' + (val % 10) as u8;
        val /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        let s = core::str::from_utf8(&buf[i..i+1]).unwrap();
        print(s);
    }
}

fn parse_response(data: &[u8]) -> Result<Response> {
    // Find end of headers
    let headers_end = find_headers_end(data)
        .ok_or_else(|| Error::http("invalid HTTP response: no header end"))?;

    let header_str = core::str::from_utf8(&data[..headers_end])
        .map_err(|_| Error::http("invalid HTTP headers"))?;

    let mut lines = header_str.lines();
    
    // Parse status line
    let status_line = lines.next()
        .ok_or_else(|| Error::http("missing status line"))?;
    
    let status = parse_status_line(status_line)?;

    // Parse headers
    let mut headers = Vec::new();
    let mut is_chunked = false;
    
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some(colon_pos) = line.find(':') {
            let name = line[..colon_pos].trim();
            let value = line[colon_pos + 1..].trim();
            
            // Check for chunked transfer encoding
            if name.eq_ignore_ascii_case("Transfer-Encoding") && value.contains("chunked") {
                is_chunked = true;
            }
            
            headers.push((String::from(name), String::from(value)));
        }
    }

    // Body is everything after headers
    let raw_body = &data[headers_end..];
    
    // Decode chunked encoding if present
    let body = if is_chunked {
        decode_chunked(raw_body)?
    } else {
        raw_body.to_vec()
    };

    Ok(Response {
        status,
        headers,
        body,
    })
}

/// Decode chunked transfer encoding
/// Format: <hex-size>\r\n<data>\r\n<hex-size>\r\n<data>\r\n...0\r\n\r\n
fn decode_chunked(data: &[u8]) -> Result<Vec<u8>> {
    let mut result = Vec::new();
    let mut pos = 0;
    
    while pos < data.len() {
        // Find end of chunk size line
        let line_end = find_crlf(&data[pos..])
            .ok_or_else(|| Error::http("invalid chunked encoding: no CRLF after size"))?;
        
        // Parse chunk size (hex)
        let size_str = core::str::from_utf8(&data[pos..pos + line_end])
            .map_err(|_| Error::http("invalid chunked encoding: size not UTF-8"))?
            .trim();
        
        // Size might have chunk extensions after semicolon, ignore them
        let size_part = size_str.split(';').next().unwrap_or(size_str).trim();
        
        let chunk_size = usize::from_str_radix(size_part, 16)
            .map_err(|_| Error::http("invalid chunked encoding: bad size"))?;
        
        // Move past size line and CRLF
        pos += line_end + 2;
        
        // Last chunk (size 0)
        if chunk_size == 0 {
            break;
        }
        
        // Ensure we have enough data
        if pos + chunk_size > data.len() {
            // Might be truncated, use what we have
            result.extend_from_slice(&data[pos..]);
            break;
        }
        
        // Copy chunk data
        result.extend_from_slice(&data[pos..pos + chunk_size]);
        pos += chunk_size;
        
        // Skip trailing CRLF after chunk data
        if pos + 2 <= data.len() && &data[pos..pos + 2] == b"\r\n" {
            pos += 2;
        }
    }
    
    Ok(result)
}

/// Find CRLF in data, returns position of \r
fn find_crlf(data: &[u8]) -> Option<usize> {
    for i in 0..data.len().saturating_sub(1) {
        if data[i] == b'\r' && data[i + 1] == b'\n' {
            return Some(i);
        }
    }
    None
}

// find_headers_end is imported from libakuma_tls

fn parse_status_line(line: &str) -> Result<u16> {
    // Format: "HTTP/1.1 200 OK"
    let mut parts = line.split_whitespace();
    let _version = parts.next()
        .ok_or_else(|| Error::http("missing HTTP version"))?;
    let status_str = parts.next()
        .ok_or_else(|| Error::http("missing status code"))?;
    
    status_str.parse::<u16>()
        .map_err(|_| Error::http("invalid status code"))
}

// Lowercase helper for no_std
trait ToLowercase {
    fn to_lowercase(&self) -> String;
}

impl ToLowercase for str {
    fn to_lowercase(&self) -> String {
        self.chars().map(|c| {
            if c.is_ascii_uppercase() {
                (c as u8 + 32) as char
            } else {
                c
            }
        }).collect()
    }
}

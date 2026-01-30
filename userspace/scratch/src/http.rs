//! HTTP client helpers for Git protocol
//!
//! Provides HTTP GET and POST for communicating with Git servers.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::net::{resolve, TcpStream};
use libakuma_tls::transport::TcpTransport;
use libakuma_tls::{TlsStream, TLS_RECORD_SIZE};

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
}

impl HttpClient {
    pub fn new(url: Url) -> Self {
        Self { url }
    }

    /// Send GET request
    pub fn get(&self, path: &str) -> Result<Response> {
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
    pub fn post(&self, path: &str, content_type: &str, body: &[u8]) -> Result<Response> {
        let request = format!(
            "POST {} HTTP/1.1\r\n\
             Host: {}\r\n\
             User-Agent: scratch/1.0\r\n\
             Content-Type: {}\r\n\
             Content-Length: {}\r\n\
             Accept: application/x-git-upload-pack-result\r\n\
             Connection: close\r\n\
             \r\n",
            path, self.url.host, content_type, body.len()
        );

        self.send_request_with_body(&request, body)
    }

    fn send_request(&self, request: &str) -> Result<Response> {
        self.send_request_with_body(request, &[])
    }

    fn send_request_with_body(&self, request: &str, body: &[u8]) -> Result<Response> {
        // Resolve host
        let ip = resolve(&self.url.host)
            .map_err(|_| Error::network("DNS resolution failed"))?;

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
                if empty_reads > 500 {
                    break;
                }
                libakuma::sleep_ms(10);
            }
            Err(_) => break,
        }
    }

    Ok(response)
}

fn read_tls_response(tls: &mut TlsStream<'_>) -> Result<Vec<u8>> {
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];

    loop {
        match tls.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }

    Ok(response)
}

fn parse_response(data: &[u8]) -> Result<Response> {
    // Find end of headers
    let headers_end = find_header_end(data)
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
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some(colon_pos) = line.find(':') {
            let name = line[..colon_pos].trim();
            let value = line[colon_pos + 1..].trim();
            headers.push((String::from(name), String::from(value)));
        }
    }

    // Body is everything after headers
    let body = data[headers_end..].to_vec();

    Ok(Response {
        status,
        headers,
        body,
    })
}

fn find_header_end(data: &[u8]) -> Option<usize> {
    for i in 0..data.len().saturating_sub(3) {
        if &data[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 4);
        }
    }
    // Try \n\n as well
    for i in 0..data.len().saturating_sub(1) {
        if &data[i..i + 2] == b"\n\n" {
            return Some(i + 2);
        }
    }
    None
}

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

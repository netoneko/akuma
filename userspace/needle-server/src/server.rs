//! HTTP/1.0 request parser and response helpers.
//! Follows the same pattern as userspace/httpd/src/main.rs.

use alloc::format;
use alloc::vec::Vec;
use libakuma::net::{Shutdown, TcpStream};

pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub body: &'a str,
}

pub fn parse_request<'a>(buf: &'a str) -> Option<Request<'a>> {
    let mut lines = buf.lines();
    let first = lines.next()?;
    let mut parts = first.split_ascii_whitespace();
    let method = parts.next()?;
    let path = parts.next()?;

    // Find body after the blank line separating headers from body
    let body = if let Some(pos) = buf.find("\r\n\r\n") {
        &buf[pos + 4..]
    } else if let Some(pos) = buf.find("\n\n") {
        &buf[pos + 2..]
    } else {
        ""
    };

    Some(Request { method, path, body })
}

pub fn send_text_plain(stream: &TcpStream, body: &[u8]) {
    let header = format!(
        "HTTP/1.0 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.shutdown(Shutdown::Write);
}

pub fn send_json(stream: &TcpStream, status: u16, reason: &str, body: &[u8]) {
    let header = format!(
        "HTTP/1.0 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.shutdown(Shutdown::Write);
}

pub fn send_chunked_start(stream: &TcpStream) {
    let header =
        "HTTP/1.0 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n";
    let _ = stream.write_all(header.as_bytes());
}

pub fn send_chunk(stream: &TcpStream, data: &[u8]) {
    let _ = stream.write_all(data);
}

pub fn send_not_implemented(stream: &TcpStream) {
    send_json(
        stream,
        501,
        "Not Implemented",
        b"{\"error\":\"not implemented\"}",
    );
}

pub fn send_not_found(stream: &TcpStream) {
    send_json(stream, 404, "Not Found", b"{\"error\":\"not found\"}");
}

pub fn send_bad_request(stream: &TcpStream, msg: &str) {
    let body = format!("{{\"error\":\"{msg}\"}}");
    send_json(stream, 400, "Bad Request", body.as_bytes());
}

pub fn send_internal_error(stream: &TcpStream) {
    send_json(
        stream,
        500,
        "Internal Server Error",
        b"{\"error\":\"internal error\"}",
    );
}

pub fn read_request(stream: &TcpStream, buf: &mut Vec<u8>) -> bool {
    let mut tmp = [0u8; 4096];
    loop {
        let n = match stream.read(&mut tmp) {
            Ok(0) | Err(_) => return false,
            Ok(n) => n,
        };
        buf.extend_from_slice(&tmp[..n]);
        // Stop reading once we have headers + body (check Content-Length)
        if let Some(header_end) = find_header_end(buf) {
            let header_str = core::str::from_utf8(&buf[..header_end]).unwrap_or("");
            let content_length = parse_content_length(header_str);
            let body_received = buf.len().saturating_sub(header_end);
            if body_received >= content_length {
                return true;
            }
        }
        if buf.len() > 8 * 1024 * 1024 {
            return false; // 8 MB hard cap
        }
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    // Find \r\n\r\n or \n\n
    for i in 0..buf.len().saturating_sub(3) {
        if buf[i] == b'\r' && buf[i+1] == b'\n' && buf[i+2] == b'\r' && buf[i+3] == b'\n' {
            return Some(i + 4);
        }
    }
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i] == b'\n' && buf[i+1] == b'\n' {
            return Some(i + 2);
        }
    }
    None
}

fn parse_content_length(headers: &str) -> usize {
    for line in headers.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(val) = lower.strip_prefix("content-length:") {
            return val.trim().parse().unwrap_or(0);
        }
    }
    0
}

//! Userspace HTTP Server
//!
//! A simple HTTP/1.0 server that serves static files from /public.
//! Supports CGI scripts in /public/cgi-bin/.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::vec;

use libakuma::net::{TcpListener, TcpStream, Error, Shutdown};
use libakuma::{print, open, read_fd, fstat, close, open_flags, lseek, seek_mode};
use libakuma::{spawn_with_env, waitpid};
#[cfg(feature = "cgi-log")]
use libakuma::write_fd;

const DEFAULT_HTTP_PORT: u16 = 8080;

/// CGI: reset idle timer after any data; bail if idle for this long
const CGI_IDLE_TIMEOUT_MS: u32 = 60_000*3;
/// CGI: hard wall-clock limit regardless of activity
const CGI_WALL_TIMEOUT_MS: u32 = 60_000*10;
/// I/O chunk size for CGI reads and body streaming
const CGI_BUF: usize = 4096;
/// Max bytes to read from the temp file when scanning for CGI headers
const CGI_HEADER_SCAN: usize = 4096;

#[no_mangle]
pub extern "C" fn main() {
    // Listen port resolution: `HTTP_PORT` env var, then the first CLI arg, then
    // the default. This lets a second instance run on a non-default port (e.g. for
    // testing a freshly-built binary alongside the autostarted server on 8080).
    let port = libakuma::env("HTTP_PORT")
        .or_else(|| libakuma::arg(1))
        .and_then(|s| s.trim().parse::<u16>().ok())
        .unwrap_or(DEFAULT_HTTP_PORT);

    print(&format!("httpd: Starting HTTP server on port {}\n", port));

    let addr = format!("0.0.0.0:{}", port);
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            print("httpd: Failed to bind: ");
            print(&format!("{:?}\n", e));
            return;
        }
    };

    print("httpd: Listening for connections...\n");

    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                handle_connection(stream);
            }
            Err(e) => {
                if e.kind != libakuma::net::ErrorKind::WouldBlock {
                    print("httpd: Accept error: ");
                    print(&format!("{:?}\n", e));
                }
                libakuma::sleep_ms(1);
            }
        }
    }
}

fn handle_connection(stream: TcpStream) {
    let mut buf = alloc::vec![0u8; 8192];
    let n = match stream.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return,
    };

    if n == 0 {
        return;
    }

    let request = match core::str::from_utf8(&buf[..n]) {
        Ok(s) => s,
        Err(_) => {
            let _ = send_error(&stream, 400, "Bad Request");
            return;
        }
    };

    let mut lines = request.lines();
    let first_line = match lines.next() {
        Some(l) => l,
        None => {
            let _ = send_error(&stream, 400, "Bad Request");
            return;
        }
    };

    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");

    let is_head = match method {
        "GET" => false,
        "HEAD" => true,
        "POST" => false,
        _ => {
            let _ = send_error(&stream, 405, "Method Not Allowed");
            return;
        }
    };

    if path.contains("..") {
        let _ = send_error(&stream, 403, "Forbidden");
        return;
    }

    if path.starts_with("/cgi-bin/") {
        let body = if method == "POST" {
            extract_post_body(&buf[..n], &stream)
        } else {
            None
        };
        handle_cgi_request(&stream, method, path, body.as_deref());
        return;
    }

    if method == "POST" {
        let _ = send_error(&stream, 405, "Method Not Allowed");
        return;
    }

    let fs_path = if path == "/" {
        String::from("/public/index.html")
    } else {
        format!("/public{}", path)
    };

    let now_us = libakuma::time();
    let time_str = format_time_rfc1123(now_us);
    print(&format!("[{}] {} {}\n", time_str, method, path));

    match read_file(&fs_path) {
        Ok(content) => {
            let content_type = get_content_type(&fs_path);
            let _ = send_file(&stream, &content, content_type, is_head);
        }
        Err(_) => {
            let _ = send_error(&stream, 404, "Not Found");
        }
    }
}

fn format_time_rfc1123(us: u64) -> String {
    let secs = us / 1_000_000;
    let mut days = secs / 86400;
    let secs_today = secs % 86400;

    let hour = (secs_today / 3600) as u8;
    let minute = ((secs_today % 3600) / 60) as u8;
    let second = (secs_today % 60) as u8;

    let wday = ((days + 4) % 7) as usize;
    let wday_str = match wday {
        0 => "Sun", 1 => "Mon", 2 => "Tue", 3 => "Wed",
        4 => "Thu", 5 => "Fri", 6 => "Sat", _ => "???"
    };

    let mut year = 1970;
    loop {
        let year_days = if is_leap_year(year) { 366 } else { 365 };
        if days < year_days { break; }
        days -= year_days;
        year += 1;
    }

    let months = if is_leap_year(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let month_strs = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun",
        "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"
    ];

    let mut month = 0;
    for (i, &month_days) in months.iter().enumerate() {
        if days < month_days as u64 { month = i; break; }
        days -= month_days as u64;
    }
    let day = (days + 1) as u8;

    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        wday_str, day, month_strs[month], year, hour, minute, second
    )
}

fn is_leap_year(year: u64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

fn extract_post_body(initial_data: &[u8], stream: &TcpStream) -> Option<Vec<u8>> {
    let request_str = core::str::from_utf8(initial_data).ok()?;

    let mut content_length: usize = 0;
    for line in request_str.lines() {
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = value.trim().parse().ok()?;
            break;
        }
        if let Some(value) = line.strip_prefix("content-length:") {
            content_length = value.trim().parse().ok()?;
            break;
        }
    }

    if content_length == 0 {
        return Some(Vec::new());
    }

    let body_start = if let Some(pos) = request_str.find("\r\n\r\n") {
        pos + 4
    } else if let Some(pos) = request_str.find("\n\n") {
        pos + 2
    } else {
        return None;
    };

    let mut body = Vec::new();
    if body_start < initial_data.len() {
        body.extend_from_slice(&initial_data[body_start..]);
    }

    let mut buf = [0u8; 1024];
    while body.len() < content_length {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let remaining = content_length - body.len();
                let to_read = n.min(remaining);
                body.extend_from_slice(&buf[..to_read]);
            }
            Err(_) => break,
        }
    }

    Some(body)
}

fn read_file(path: &str) -> Result<Vec<u8>, i32> {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        return Err(-fd);
    }

    let stat = match fstat(fd) {
        Ok(s) => s,
        Err(e) => { close(fd); return Err(e); }
    };

    let size = stat.st_size as usize;
    lseek(fd, 0, seek_mode::SEEK_SET);

    let mut content = alloc::vec![0u8; size];
    let mut read = 0;
    while read < size {
        let n = read_fd(fd, &mut content[read..]);
        if n <= 0 { break; }
        read += n as usize;
    }

    close(fd);
    Ok(content)
}

fn get_content_type(path: &str) -> &'static str {
    if path.ends_with(".html") || path.ends_with(".htm") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".json") {
        "application/json; charset=utf-8"
    } else if path.ends_with(".txt") {
        "text/plain; charset=utf-8"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        "image/jpeg"
    } else if path.ends_with(".gif") {
        "image/gif"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else {
        "application/octet-stream"
    }
}

fn send_file(stream: &TcpStream, content: &[u8], content_type: &str, head_only: bool) -> Result<(), Error> {
    let date = format_time_rfc1123(libakuma::time());
    let response = format!(
        "HTTP/1.0 200 OK\r\n\
         Date: {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        date, content_type, content.len()
    );

    stream.write_all(response.as_bytes())?;
    if !head_only {
        stream.write_all(content)?;
    }
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
}

fn send_error(stream: &TcpStream, code: u16, message: &str) -> Result<(), Error> {
    let body = format!(
        "<!DOCTYPE html>\n<html><head><title>{} {}</title></head>\n\
         <body><h1>{} {}</h1></body></html>\n",
        code, message, code, message
    );

    let date = format_time_rfc1123(libakuma::time());
    let response = format!(
        "HTTP/1.0 {} {}\r\n\
         Date: {}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        code, message, date, body.len(), body
    );

    stream.write_all(response.as_bytes())?;
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
}

// ============================================================================
// CGI Support
// ============================================================================

fn get_interpreter(script_path: &str) -> Option<&'static str> {
    if script_path.ends_with(".js") {
        Some("/bin/qjs")
    } else {
        None
    }
}

fn parse_path_and_query(path: &str) -> (&str, Option<&str>) {
    if let Some(pos) = path.find('?') {
        let (script_path, query_with_marker) = path.split_at(pos);
        let query = &query_with_marker[1..];
        (script_path, if query.is_empty() { None } else { Some(query) })
    } else {
        (path, None)
    }
}

/// Handle a CGI request with streaming output.
///
/// Phase 1: buffer CGI output until the header/body boundary is found, then
/// send HTTP response headers and flush any already-buffered body bytes.
/// Phase 2: pipe remaining CGI stdout directly to the HTTP client as it arrives.
/// No temp file — output reaches the client as soon as the CGI script writes it.
fn handle_cgi_request(stream: &TcpStream, method: &str, path: &str, body: Option<&[u8]>) {
    let (script_path, query_string) = parse_path_and_query(path);
    let fs_path = format!("/public{}", script_path);

    let fd = open(&fs_path, open_flags::O_RDONLY);
    if fd < 0 {
        let _ = send_error(stream, 404, "Not Found");
        return;
    }
    close(fd);

    let now_us = libakuma::time();
    let time_str = format_time_rfc1123(now_us);
    print(&format!("[{}] CGI {} {}\n", time_str, method, path));

    let interpreter = get_interpreter(&fs_path);
    let query_str = query_string.unwrap_or("");

    let method_env = format!("REQUEST_METHOD={}", method);
    let query_env = format!("QUERY_STRING={}", query_str);
    let cgi_env: &[&str] = &[method_env.as_str(), query_env.as_str()];

    let spawn_result = if let Some(interp) = interpreter {
        let args: Vec<&str> = vec![&fs_path, method, query_str];
        spawn_with_env(interp, Some(&args), body, cgi_env)
    } else {
        let args: Vec<&str> = vec![method, query_str];
        spawn_with_env(&fs_path, Some(&args), body, cgi_env)
    };

    let result = match spawn_result {
        Some(r) => r,
        None => {
            let _ = send_error(stream, 500, "Internal Server Error");
            return;
        }
    };

    let stdout = result.stdout_fd as i32;
    let mut io_buf = alloc::vec![0u8; CGI_BUF];
    let mut idle_ms: u32 = 0;
    let mut total_ms: u32 = 0;

    #[cfg(feature = "cgi-log")]
    let tmp_path = alloc::format!("/tmp/cgi_{}.out", result.pid);
    #[cfg(feature = "cgi-log")]
    let tmp_fd = open(&tmp_path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);

    // Phase 1: accumulate output until the CGI header/body boundary appears.
    let mut header_buf: Vec<u8> = Vec::new();
    let mut body_offset: usize = 0;
    let mut process_exited = false;
    let mut timed_out = false;

    'header: loop {
        let n = read_fd(stdout, &mut io_buf);
        if n > 0 {
            idle_ms = 0;
            header_buf.extend_from_slice(&io_buf[..n as usize]);
            #[cfg(feature = "cgi-log")]
            if tmp_fd >= 0 { write_fd(tmp_fd, &io_buf[..n as usize]); }
            if let Some(off) = cgi_boundary(&header_buf) {
                body_offset = off;
                break 'header;
            }
            if header_buf.len() >= CGI_HEADER_SCAN {
                break 'header; // no CGI headers — treat whole buffer as body
            }
        } else {
            if let Some(_) = waitpid(result.pid) {
                process_exited = true;
                loop {
                    let n = read_fd(stdout, &mut io_buf);
                    if n <= 0 { break; }
                    header_buf.extend_from_slice(&io_buf[..n as usize]);
                    #[cfg(feature = "cgi-log")]
                    if tmp_fd >= 0 { write_fd(tmp_fd, &io_buf[..n as usize]); }
                }
                if let Some(off) = cgi_boundary(&header_buf) {
                    body_offset = off;
                }
                break 'header;
            }
            if idle_ms >= CGI_IDLE_TIMEOUT_MS || total_ms >= CGI_WALL_TIMEOUT_MS {
                timed_out = true;
                break 'header;
            }
            libakuma::sleep_ms(1);
            idle_ms += 1;
            total_ms += 1;
        }
    }

    if timed_out {
        close(stdout);
        let _ = send_error(stream, 504, "Gateway Timeout");
        return;
    }

    let (content_type, _) = parse_cgi_headers(&header_buf);

    // Send HTTP response headers — no Content-Length since we stream
    let date = format_time_rfc1123(libakuma::time());
    let http_header = format!(
        "HTTP/1.0 200 OK\r\n\
         Date: {}\r\n\
         Content-Type: {}\r\n\
         Connection: close\r\n\
         \r\n",
        date, content_type
    );
    if stream.write_all(http_header.as_bytes()).is_err() {
        close(stdout);
        return;
    }

    // Flush body bytes already in the header buffer
    if body_offset < header_buf.len() {
        if stream.write_all(&header_buf[body_offset..]).is_err() {
            close(stdout);
            return;
        }
    }

    // Phase 2: stream remaining CGI output directly to the client
    if !process_exited {
        idle_ms = 0;
        loop {
            let n = read_fd(stdout, &mut io_buf);
            if n > 0 {
                idle_ms = 0;
                #[cfg(feature = "cgi-log")]
                if tmp_fd >= 0 { write_fd(tmp_fd, &io_buf[..n as usize]); }
                if stream.write_all(&io_buf[..n as usize]).is_err() {
                    break;
                }
            } else {
                if let Some(_) = waitpid(result.pid) {
                    break;
                }
                if idle_ms >= CGI_IDLE_TIMEOUT_MS || total_ms >= CGI_WALL_TIMEOUT_MS {
                    break;
                }
                libakuma::sleep_ms(1);
                idle_ms += 1;
                total_ms += 1;
            }
        }
    }

    close(stdout);
    #[cfg(feature = "cgi-log")]
    if tmp_fd >= 0 { close(tmp_fd); }
    let _ = stream.shutdown(Shutdown::Write);
}

fn cgi_boundary(data: &[u8]) -> Option<usize> {
    // scan as bytes to avoid UTF-8 conversion overhead on each chunk
    let len = data.len();
    if len >= 4 {
        for i in 0..len - 3 {
            if data[i] == b'\r' && data[i+1] == b'\n' && data[i+2] == b'\r' && data[i+3] == b'\n' {
                return Some(i + 4);
            }
        }
    }
    if len >= 2 {
        for i in 0..len - 1 {
            if data[i] == b'\n' && data[i+1] == b'\n' {
                return Some(i + 2);
            }
        }
    }
    None
}

/// Find the CGI header/body boundary in the first scan_bytes of output.
/// Returns (content_type, body_start_offset).
fn parse_cgi_headers(scan_bytes: &[u8]) -> (&'static str, usize) {
    let scan_str = match core::str::from_utf8(scan_bytes) {
        Ok(s) => s,
        Err(_) => return ("application/octet-stream", 0),
    };

    let (header_end, body_start) = if let Some(pos) = scan_str.find("\r\n\r\n") {
        (pos, pos + 4)
    } else if let Some(pos) = scan_str.find("\n\n") {
        (pos, pos + 2)
    } else {
        return ("text/plain", 0);
    };

    let headers = &scan_str[..header_end];
    let mut content_type = "text/plain";
    for line in headers.lines() {
        if let Some(v) = line.strip_prefix("Content-Type:") {
            content_type = v.trim();
            break;
        }
        if let Some(v) = line.strip_prefix("content-type:") {
            content_type = v.trim();
            break;
        }
    }

    // content_type points into scan_str (stack buffer) — can't return a reference
    // to it safely. Use a static match for common types; fall back to text/plain.
    let static_type = match content_type {
        t if t.starts_with("text/html") => "text/html; charset=utf-8",
        t if t.starts_with("text/plain") => "text/plain; charset=utf-8",
        t if t.starts_with("application/json") => "application/json; charset=utf-8",
        t if t.starts_with("application/javascript") => "application/javascript; charset=utf-8",
        _ => "text/plain; charset=utf-8",
    };

    (static_type, body_start)
}

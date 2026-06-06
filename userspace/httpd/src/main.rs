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
use libakuma::{print, open, read_fd, write_fd, fstat, close, open_flags, lseek, seek_mode};
use libakuma::{spawn_with_env, waitpid, unlink};

const HTTP_PORT: u16 = 8080;

/// CGI: reset idle timer after any data; bail if idle for this long
const CGI_IDLE_TIMEOUT_MS: u32 = 60_000;
/// CGI: hard wall-clock limit regardless of activity
const CGI_WALL_TIMEOUT_MS: u32 = 300_000;
/// I/O chunk size for CGI reads and body streaming
const CGI_BUF: usize = 4096;
/// Max bytes to read from the temp file when scanning for CGI headers
const CGI_HEADER_SCAN: usize = 4096;

#[no_mangle]
pub extern "C" fn main() {
    print("httpd: Starting HTTP server on port 8080\n");

    let addr = format!("0.0.0.0:{}", HTTP_PORT);
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
    let mut buf = [0u8; 8192];
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

/// Handle a CGI request.
/// CGI output is spooled to /tmp/cgi_<pid>.out (file-based, no heap Vec),
/// then streamed to the client chunk by chunk.
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

    // Standard CGI environment variables
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

    // Spool CGI stdout to a temp file — never buffers the whole response in heap
    let tmp_path = format!("/tmp/cgi_{}.out", result.pid);
    let tmp_fd = open(
        &tmp_path,
        open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC,
    );
    if tmp_fd < 0 {
        let _ = send_error(stream, 500, "Internal Server Error");
        return;
    }

    let mut io_buf = [0u8; CGI_BUF];
    let mut process_exited = false;
    let mut idle_ms: u32 = 0;
    let mut total_ms: u32 = 0;

    loop {
        // Drain all available data from CGI stdout
        let mut got_data = false;
        loop {
            let n = read_fd(result.stdout_fd as i32, &mut io_buf);
            if n > 0 {
                write_fd(tmp_fd, &io_buf[..n as usize]);
                got_data = true;
            } else {
                break;
            }
        }

        if got_data {
            idle_ms = 0;
        }

        // Check if process exited
        if let Some(_) = waitpid(result.pid) {
            process_exited = true;
            // Final drain
            loop {
                let n = read_fd(result.stdout_fd as i32, &mut io_buf);
                if n <= 0 { break; }
                write_fd(tmp_fd, &io_buf[..n as usize]);
            }
            break;
        }

        if idle_ms >= CGI_IDLE_TIMEOUT_MS || total_ms >= CGI_WALL_TIMEOUT_MS {
            break;
        }

        libakuma::sleep_ms(1);
        idle_ms += 1;
        total_ms += 1;
    }

    close(result.stdout_fd as i32);
    close(tmp_fd);

    if !process_exited {
        unlink(&tmp_path);
        let _ = send_error(stream, 504, "Gateway Timeout");
        return;
    }

    // Parse CGI headers from the start of the temp file
    let header_fd = open(&tmp_path, open_flags::O_RDONLY);
    if header_fd < 0 {
        let _ = send_error(stream, 500, "Internal Server Error");
        return;
    }

    let mut scan_buf = [0u8; CGI_HEADER_SCAN];
    let scan_n = read_fd(header_fd, &mut scan_buf);
    close(header_fd);

    let scan_bytes = if scan_n > 0 { &scan_buf[..scan_n as usize] } else { &[] };
    let (content_type, body_offset) = parse_cgi_headers(scan_bytes);

    // Compute body size from temp file stat
    let stat_fd = open(&tmp_path, open_flags::O_RDONLY);
    if stat_fd < 0 {
        unlink(&tmp_path);
        let _ = send_error(stream, 500, "Internal Server Error");
        return;
    }
    let file_size = match fstat(stat_fd) {
        Ok(s) => s.st_size as i64,
        Err(_) => {
            close(stat_fd);
            unlink(&tmp_path);
            let _ = send_error(stream, 500, "Internal Server Error");
            return;
        }
    };
    close(stat_fd);

    let body_size = (file_size - body_offset as i64).max(0) as u64;

    // Send HTTP response headers
    let date = format_time_rfc1123(libakuma::time());
    let http_header = format!(
        "HTTP/1.0 200 OK\r\n\
         Date: {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        date, content_type, body_size
    );
    if stream.write_all(http_header.as_bytes()).is_err() {
        unlink(&tmp_path);
        return;
    }

    // Stream body from temp file in 4 KB chunks — heap peak = one buffer
    let body_fd = open(&tmp_path, open_flags::O_RDONLY);
    if body_fd >= 0 {
        lseek(body_fd, body_offset as i64, seek_mode::SEEK_SET);
        let mut send_buf = [0u8; CGI_BUF];
        loop {
            let n = read_fd(body_fd, &mut send_buf);
            if n <= 0 { break; }
            if stream.write_all(&send_buf[..n as usize]).is_err() { break; }
        }
        close(body_fd);
    }

    let _ = stream.shutdown(Shutdown::Write);
    unlink(&tmp_path);
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

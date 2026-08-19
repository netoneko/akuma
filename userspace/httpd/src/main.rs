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

mod resp;
mod stats;
use resp::{DateCache, FixedBuf};
use stats::{Phase, Stats};

use core::fmt::Write as _;

/// Request buffer size. Was an 8 KB `alloc::vec!` per connection; it is a single
/// reusable buffer now because `httpd` serves one connection at a time (see the
/// accept loop in `main`), so there is never a second live request to overlap
/// with it.
const REQUEST_BUF: usize = 8192;

/// Files at or below this size are served out of a reusable buffer with no
/// allocation. Above it, `read_file` falls back to a `Vec` — a large download is
/// dominated by the transfer, not by one `malloc`, and sizing the static buffer
/// for the worst case would cost that memory permanently.
const STATIC_FILE_BUF: usize = 64 * 1024;

/// Longest filesystem path this server will build. Anything longer is refused
/// with a 403 rather than truncated — a truncated path resolves to a different
/// file, which is a correctness bug wearing a buffer-size costume.
const PATH_BUF: usize = 512;

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

    // Phase timing for the request path (`HTTPD_STATS=1`). See stats.rs for what
    // each phase covers and why `other` is the "server compute" number.
    let mut st = Stats::new();
    // One cache for the process: the `Date` header only changes once a second.
    let mut dates = DateCache::new();
    // One reusable request buffer, reused across connections (see REQUEST_BUF).
    let mut req = alloc::vec![0u8; REQUEST_BUF];
    // One reusable file buffer for small responses (see STATIC_FILE_BUF).
    let mut file = alloc::vec![0u8; STATIC_FILE_BUF];

    loop {
        st.begin();
        match listener.accept() {
            Ok((stream, addr)) => {
                // Charged to `accept`: everything from the previous request
                // finishing to this connection existing. On Akuma that is a
                // kernel `wait_until` parked in WFI, so it is the phase most
                // likely to dominate — which is exactly the point of measuring.
                st.mark(Phase::Accept);
                if st.verbose() {
                    print(&format!("httpd: connection from {}\n", libakuma::net::format_addr(&addr)));
                }
                st.mark(Phase::Log);
                handle_connection(stream, &mut st, &mut dates, &mut req, &mut file);
                st.finish_request();
            }
            Err(e) => {
                st.mark(Phase::Accept);
                if e.kind != libakuma::net::ErrorKind::WouldBlock {
                    print("httpd: Accept error: ");
                    print(&format!("{:?}\n", e));
                }
                libakuma::sleep_ms(1);
            }
        }
    }
}

fn handle_connection(
    stream: TcpStream,
    st: &mut Stats,
    dates: &mut DateCache,
    buf: &mut [u8],
    file: &mut [u8],
) {
    let n = match stream.read(buf) {
        Ok(n) => n,
        Err(_) => return,
    };
    st.mark(Phase::Read);

    if n == 0 {
        return;
    }

    let request = match core::str::from_utf8(&buf[..n]) {
        Ok(s) => s,
        Err(_) => {
            let _ = send_error(&stream, 400);
            return;
        }
    };

    let mut lines = request.lines();
    let first_line = match lines.next() {
        Some(l) => l,
        None => {
            let _ = send_error(&stream, 400);
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
            let _ = send_error(&stream, 405);
            return;
        }
    };

    if path.contains("..") {
        let _ = send_error(&stream, 403);
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
        let _ = send_error(&stream, 405);
        return;
    }

    // Filesystem path, built into a stack buffer rather than a `String`. A path
    // longer than the buffer is refused instead of being silently truncated,
    // which would otherwise resolve to some *other* file.
    let mut fs_path = FixedBuf::<PATH_BUF>::new();
    let _ = fs_path.write_str("/public");
    let _ = fs_path.write_str(if path == "/" { "/index.html" } else { path });
    if fs_path.truncated() {
        let _ = send_error(&stream, 403);
        return;
    }
    let Ok(fs_path) = core::str::from_utf8(fs_path.as_bytes()) else {
        let _ = send_error(&stream, 400);
        return;
    };

    // Everything above this point is parse + path building: charge it to the
    // compute bucket before the logging and I/O phases start.
    st.mark(Phase::Other);

    if st.verbose() {
        let now_us = libakuma::time();
        let time_str = format_time_rfc1123(now_us);
        print(&format!("[{}] {} {}\n", time_str, method, path));
    }
    st.mark(Phase::Log);

    let content_type = get_content_type(fs_path);
    match serve_file(&stream, fs_path, content_type, is_head, dates, file, st) {
        Ok(n) => st.add_tx(n),
        Err(_) => {
            let _ = send_error(&stream, 404);
            st.mark(Phase::Write);
        }
    }
}

/// Open `path`, send a 200 with its contents, and return the body length.
///
/// Reads through the caller's reusable buffer instead of allocating a `Vec` per
/// request. A file larger than that buffer is **streamed** in buffer-sized
/// chunks rather than growing the buffer: `Content-Length` comes from `fstat`,
/// so the response is still framed correctly without ever holding the whole file
/// in memory.
fn serve_file(
    stream: &TcpStream,
    path: &str,
    content_type: &str,
    head_only: bool,
    dates: &mut DateCache,
    buf: &mut [u8],
    st: &mut Stats,
) -> Result<usize, i32> {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        st.mark(Phase::File);
        return Err(-fd);
    }
    let stat = match fstat(fd) {
        Ok(s) => s,
        Err(e) => {
            close(fd);
            st.mark(Phase::File);
            return Err(e);
        }
    };
    let size = stat.st_size as usize;
    lseek(fd, 0, seek_mode::SEEK_SET);
    st.mark(Phase::File);

    if send_headers(stream, content_type, size, dates).is_err() {
        close(fd);
        st.mark(Phase::Write);
        return Ok(0);
    }
    if head_only {
        close(fd);
        let _ = stream.shutdown(Shutdown::Write);
        st.mark(Phase::Write);
        return Ok(0);
    }

    let mut sent = 0usize;
    while sent < size {
        let want = (size - sent).min(buf.len());
        let n = read_fd(fd, &mut buf[..want]);
        st.mark(Phase::File);
        if n <= 0 {
            break;
        }
        let n = n as usize;
        if stream.write_all(&buf[..n]).is_err() {
            break;
        }
        st.mark(Phase::Write);
        sent += n;
    }
    close(fd);
    let _ = stream.shutdown(Shutdown::Write);
    st.mark(Phase::Write);
    Ok(sent)
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
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

fn extract_post_body(initial_data: &[u8], stream: &TcpStream) -> Option<Vec<u8>> {
    let request_str = core::str::from_utf8(initial_data).ok()?;

    let mut content_length: usize = 0;
    for line in request_str.lines() {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().ok()?;
                break;
            }
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

/// Send a 200 with `content` as the body.
///
/// The header block is formatted into a stack buffer instead of a `String`:
/// only `Content-Type`, `Content-Length` and `Date` vary, and `Date` comes from
/// a per-second cache (`DateCache`) rather than being recomputed per request.
/// Write a 200 header block.
///
/// Formatted into a stack buffer instead of a `String`: only `Content-Type`,
/// `Content-Length` and `Date` vary, and `Date` comes from a per-second cache
/// (`DateCache`) rather than being recomputed per request.
fn send_headers(
    stream: &TcpStream,
    content_type: &str,
    content_len: usize,
    dates: &mut DateCache,
) -> Result<(), Error> {
    let mut hdr = FixedBuf::<256>::new();
    let _ = hdr.write_str("HTTP/1.0 200 OK\r\nDate: ");
    // The cache holds ASCII produced by `write_rfc1123`, so this is infallible;
    // a corrupt cache degrades to an absent Date rather than a failed response.
    if let Ok(d) = core::str::from_utf8(dates.get(libakuma::time())) {
        let _ = hdr.write_str(d);
    }
    let _ = write!(
        hdr,
        "\r\nContent-Type: {content_type}\r\nContent-Length: {content_len}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(hdr.as_bytes())
}

/// Send one of the compile-time error responses.
///
/// `message` is no longer a parameter: the text belongs to the status code and
/// lives in `resp::ERROR_RESPONSES` alongside a `Content-Length` computed from
/// the body it describes. Passing it in was how the two could disagree.
fn send_error(stream: &TcpStream, code: u16) -> Result<(), Error> {
    stream.write_all(resp::error_response(code))?;
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
        let _ = send_error(stream, 404);
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
            let _ = send_error(stream, 500);
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
            if waitpid(result.pid).is_some() {
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
        let _ = send_error(stream, 504);
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
    if body_offset < header_buf.len() && stream.write_all(&header_buf[body_offset..]).is_err() {
        close(stdout);
        return;
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
                if waitpid(result.pid).is_some() {
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

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

use libakuma::net::{TcpListener, TcpStream, Error};
use libakuma::{print, exit, open, read_fd, fstat, close, open_flags, lseek, seek_mode};
use libakuma::{spawn_with_stdin, waitpid};

const HTTP_PORT: u16 = 8080;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    main();
    exit(0);
}

fn main() {
    print("httpd: Starting HTTP server on port 8080\n");

    // Bind to all interfaces
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

    // Accept loop
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
                // Yield before retrying
                libakuma::sleep_ms(10);
            }
        }
    }
}

fn handle_connection(stream: TcpStream) {
    // Read request (may need multiple reads for large POST bodies)
    let mut buf = [0u8; 8192];
    let n = match stream.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return,
    };

    if n == 0 {
        return;
    }

    // Parse request line
    let request = match core::str::from_utf8(&buf[..n]) {
        Ok(s) => s,
        Err(_) => {
            let _ = send_error(&stream, 400, "Bad Request");
            return;
        }
    };

    // Extract method and path
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

    // Support GET, HEAD, and POST
    let is_head = match method {
        "GET" => false,
        "HEAD" => true,
        "POST" => false,
        _ => {
            let _ = send_error(&stream, 405, "Method Not Allowed");
            return;
        }
    };

    // Security: prevent directory traversal
    if path.contains("..") {
        let _ = send_error(&stream, 403, "Forbidden");
        return;
    }

    // Check for CGI request
    if path.starts_with("/cgi-bin/") {
        // For POST requests, extract the body
        let body = if method == "POST" {
            extract_post_body(&buf[..n], &stream)
        } else {
            None
        };
        handle_cgi_request(&stream, method, path, body.as_deref());
        return;
    }

    // POST only allowed for CGI
    if method == "POST" {
        let _ = send_error(&stream, 405, "Method Not Allowed");
        return;
    }

    // Map path to filesystem
    let fs_path = if path == "/" {
        String::from("/public/index.html")
    } else {
        format!("/public{}", path)
    };

    // Try to read the file
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

/// Extract POST body from request.
/// Parses Content-Length header and reads the body.
fn extract_post_body(initial_data: &[u8], stream: &TcpStream) -> Option<Vec<u8>> {
    let request_str = core::str::from_utf8(initial_data).ok()?;
    
    // Find Content-Length header
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
    
    // Find the body (after \r\n\r\n or \n\n)
    let body_start = if let Some(pos) = request_str.find("\r\n\r\n") {
        pos + 4
    } else if let Some(pos) = request_str.find("\n\n") {
        pos + 2
    } else {
        return None;
    };
    
    let mut body = Vec::new();
    
    // Copy any body data already in initial_data
    if body_start < initial_data.len() {
        body.extend_from_slice(&initial_data[body_start..]);
    }
    
    // Read more data if needed
    let mut buf = [0u8; 1024];
    while body.len() < content_length {
        match stream.read(&mut buf) {
            Ok(0) => break, // EOF
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
    // Open file
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        return Err(-fd);
    }

    // Get file size
    let stat = match fstat(fd) {
        Ok(s) => s,
        Err(e) => {
            close(fd);
            return Err(e);
        }
    };

    let size = stat.st_size as usize;

    // Seek to beginning
    lseek(fd, 0, seek_mode::SEEK_SET);

    // Read content
    let mut content = alloc::vec![0u8; size];
    let mut read = 0;
    while read < size {
        let n = read_fd(fd, &mut content[read..]);
        if n <= 0 {
            break;
        }
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
    let response = format!(
        "HTTP/1.0 200 OK\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        content_type,
        content.len()
    );

    stream.write_all(response.as_bytes())?;

    if !head_only {
        stream.write_all(content)?;
    }

    Ok(())
}

fn send_error(stream: &TcpStream, code: u16, message: &str) -> Result<(), Error> {
    let body = format!(
        "<!DOCTYPE html>\n<html><head><title>{} {}</title></head>\n\
         <body><h1>{} {}</h1></body></html>\n",
        code, message, code, message
    );

    let response = format!(
        "HTTP/1.0 {} {}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        code, message, body.len(), body
    );

    stream.write_all(response.as_bytes())
}

// ============================================================================
// CGI Support
// ============================================================================

/// Get interpreter for a CGI script based on file extension.
/// Returns Some(interpreter_path) if the script needs an interpreter,
/// or None if it should be executed directly as an ELF binary.
fn get_interpreter(script_path: &str) -> Option<&'static str> {
    if script_path.ends_with(".js") {
        Some("/bin/qjs")
    } else {
        None // Execute directly as ELF binary
    }
}

/// Parse path and query string from a URL path.
/// E.g., "/cgi-bin/hello.js?name=world" -> ("/cgi-bin/hello.js", Some("name=world"))
fn parse_path_and_query(path: &str) -> (&str, Option<&str>) {
    if let Some(pos) = path.find('?') {
        let (script_path, query_with_marker) = path.split_at(pos);
        // Skip the '?' character
        let query = &query_with_marker[1..];
        (script_path, if query.is_empty() { None } else { Some(query) })
    } else {
        (path, None)
    }
}

/// Handle a CGI request by executing the script and returning its output.
fn handle_cgi_request(stream: &TcpStream, method: &str, path: &str, body: Option<&[u8]>) {
    // Parse path and query string
    let (script_path, query_string) = parse_path_and_query(path);
    
    // Map URL path to filesystem path
    let fs_path = format!("/public{}", script_path);
    
    // Check if the script exists
    let fd = open(&fs_path, open_flags::O_RDONLY);
    if fd < 0 {
        let _ = send_error(stream, 404, "Not Found");
        return;
    }
    close(fd);
    
    // Determine if we need an interpreter
    let interpreter = get_interpreter(&fs_path);
    
    // Build arguments for the CGI script
    // For interpreted scripts: interpreter script_path METHOD [QUERY_STRING]
    // For ELF binaries: binary METHOD [QUERY_STRING]
    let query_str = query_string.unwrap_or("");
    
    let spawn_result = if let Some(interp) = interpreter {
        // Interpreted script: spawn interpreter with script as argument
        let args: Vec<&str> = vec![&fs_path, method, query_str];
        spawn_with_stdin(interp, Some(&args), body)
    } else {
        // ELF binary: spawn directly
        let args: Vec<&str> = vec![method, query_str];
        spawn_with_stdin(&fs_path, Some(&args), body)
    };
    
    let result = match spawn_result {
        Some(r) => r,
        None => {
            let _ = send_error(stream, 500, "Internal Server Error");
            return;
        }
    };
    
    // Read output from child process, polling until process exits
    let mut output = Vec::new();
    let mut buf = [0u8; 1024];
    let mut process_exited = false;
    let mut attempts = 0;
    const MAX_ATTEMPTS: u32 = 5000; // 5 seconds max
    
    while attempts < MAX_ATTEMPTS {
        // Try to read any available output
        let n = read_fd(result.stdout_fd as i32, &mut buf);
        if n > 0 {
            output.extend_from_slice(&buf[..n as usize]);
        }
        
        // Check if process has exited
        if let Some((_pid, _exit_code)) = waitpid(result.pid) {
            process_exited = true;
            // Read any remaining output after process exit
            loop {
                let n = read_fd(result.stdout_fd as i32, &mut buf);
                if n <= 0 {
                    break;
                }
                output.extend_from_slice(&buf[..n as usize]);
            }
            break;
        }
        
        // Sleep briefly before next poll
        libakuma::sleep_ms(1);
        attempts += 1;
    }
    
    // Close the stdout fd
    close(result.stdout_fd as i32);
    
    // Send the CGI response
    if process_exited {
        let _ = send_cgi_response(stream, &output);
    } else {
        // Process timed out
        let _ = send_error(stream, 504, "Gateway Timeout");
    }
}

/// Parse CGI headers and body from script output.
/// Returns (content_type, body) where body is the content after headers.
fn parse_cgi_output(output: &[u8]) -> (&str, &[u8]) {
    // Convert to string for header parsing
    let output_str = match core::str::from_utf8(output) {
        Ok(s) => s,
        Err(_) => return ("application/octet-stream", output),
    };
    
    // Look for blank line separating headers from body
    // Try \r\n\r\n first, then \n\n
    let (header_end, body_start) = if let Some(pos) = output_str.find("\r\n\r\n") {
        (pos, pos + 4)
    } else if let Some(pos) = output_str.find("\n\n") {
        (pos, pos + 2)
    } else {
        // No headers found, treat entire output as body
        return ("text/plain", output);
    };
    
    let headers = &output_str[..header_end];
    let body = &output.as_ref()[body_start..];
    
    // Parse Content-Type from headers
    let mut content_type = "text/plain";
    for line in headers.lines() {
        if let Some(value) = line.strip_prefix("Content-Type:") {
            content_type = value.trim();
            break;
        }
        // Also check lowercase
        if let Some(value) = line.strip_prefix("content-type:") {
            content_type = value.trim();
            break;
        }
    }
    
    (content_type, body)
}

/// Send CGI output as an HTTP response.
fn send_cgi_response(stream: &TcpStream, output: &[u8]) -> Result<(), Error> {
    let (content_type, body) = parse_cgi_output(output);
    
    let response = format!(
        "HTTP/1.0 200 OK\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        content_type,
        body.len()
    );

    stream.write_all(response.as_bytes())?;
    stream.write_all(body)?;
    Ok(())
}

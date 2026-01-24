//! Userspace HTTP Server
//!
//! A simple HTTP/1.0 server that serves static files from /public.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::net::{TcpListener, TcpStream, Error};
use libakuma::{print, exit, open, read_fd, fstat, close, open_flags, lseek, seek_mode};

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
    print("httpd: Connection accepted, reading request...\n");
    
    // Read request
    let mut buf = [0u8; 1024];
    let n = match stream.read(&mut buf) {
        Ok(n) => {
            print(&format!("httpd: Read {} bytes\n", n));
            n
        }
        Err(e) => {
            print(&format!("httpd: Read error: {:?}\n", e));
            return;
        }
    };

    if n == 0 {
        print("httpd: Empty request, closing\n");
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

    // Only support GET and HEAD
    let is_head = match method {
        "GET" => false,
        "HEAD" => true,
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

    // Map path to filesystem
    let fs_path = if path == "/" {
        String::from("/public/index.html")
    } else {
        format!("/public{}", path)
    };

    // Try to read the file
    print(&format!("httpd: Serving file: {}\n", fs_path));
    match read_file(&fs_path) {
        Ok(content) => {
            print(&format!("httpd: File size: {} bytes\n", content.len()));
            let content_type = get_content_type(&fs_path);
            match send_file(&stream, &content, content_type, is_head) {
                Ok(()) => print("httpd: Response sent successfully\n"),
                Err(e) => print(&format!("httpd: Send error: {:?}\n", e)),
            }
        }
        Err(e) => {
            print(&format!("httpd: File not found: {} (err={})\n", fs_path, e));
            let _ = send_error(&stream, 404, "Not Found");
        }
    }
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

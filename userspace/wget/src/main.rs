//! Userspace wget - HTTP file downloader
//!
//! Usage: wget <url> [output_file]
//!
//! Examples:
//!   wget http://example.com/file.txt
//!   wget http://192.168.1.1:8080/data.json output.json

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::net::{TcpStream, resolve};
use libakuma::{print, exit, arg, argc, open, write_fd, close, open_flags, SocketAddrV4};

#[no_mangle]
pub extern "C" fn main() {
    // Parse arguments
    if argc() < 2 {
        print("Usage: wget <url> [output_file]\n");
        print("Example: wget http://example.com/file.txt\n");
        exit(1);
    }

    let url = match arg(1) {
        Some(u) => u,
        None => {
            print("wget: missing URL\n");
            exit(1);
        }
    };

    // Parse URL
    let parsed = match parse_url(url) {
        Some(p) => p,
        None => {
            print("wget: invalid URL format\n");
            print("Expected: http://host[:port]/path\n");
            exit(1);
        }
    };

    print("wget: Connecting to ");
    print(parsed.host);
    print(":");
    print_num(parsed.port as usize);
    print("\n");

    // Resolve hostname to IP
    let ip = match resolve(parsed.host) {
        Ok(ip) => ip,
        Err(_) => {
            print("wget: DNS resolution failed\n");
            exit(1);
        }
    };

    print("wget: Resolved to ");
    print_ip(ip);
    print("\n");

    // Connect
    let addr_str = format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], parsed.port);
    print("wget: Connecting to address: ");
    print(&addr_str);
    print("\n");
    
    let stream = match TcpStream::connect(&addr_str) {
        Ok(s) => s,
        Err(e) => {
            print("wget: Connection failed: ");
            print(&format!("{:?}\n", e));
            exit(1);
        }
    };

    // Send HTTP request
    let request = format!(
        "GET {} HTTP/1.0\r\n\
         Host: {}\r\n\
         User-Agent: wget/1.0 (Akuma)\r\n\
         Connection: close\r\n\
         \r\n",
        parsed.path,
        parsed.host
    );

    print("wget: Connected, sending request:\n");
    print(&request);

    if let Err(e) = stream.write_all(request.as_bytes()) {
        print("wget: Failed to send request: ");
        print(&format!("{:?}\n", e));
        exit(1);
    }

    // Read response
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => {
                response.extend_from_slice(&buf[..n]);
            }
            Err(e) => {
                if e.kind == libakuma::net::ErrorKind::WouldBlock || e.kind == libakuma::net::ErrorKind::TimedOut {
                    // Kernel recv already blocks, break on timeout.
                    break;
                }
                print("wget: Read error: ");
                print(&format!("{:?}\n", e));
                break;
            }
        }
    }

    print("wget: Received ");
    print_num(response.len());
    print(" bytes\n");

    // Parse HTTP response
    let (status, headers_end, _body) = match parse_response(&response) {
        Some(r) => r,
        None => {
            print("wget: Failed to parse HTTP response\n");
            exit(1);
        }
    };

    print("wget: HTTP status ");
    print_num(status as usize);
    print("\n");

    if status != 200 {
        print("wget: Server returned error\n");
        exit(1);
    }

    // Determine output filename
    let output_file = if argc() >= 3 {
        match arg(2) {
            Some(f) => String::from(f),
            None => extract_filename(parsed.path),
        }
    } else {
        extract_filename(parsed.path)
    };

    print("wget: Saving to ");
    print(&output_file);
    print("\n");

    // Write body to file
    let fd = open(&output_file, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd < 0 {
        print("wget: Failed to create output file\n");
        exit(1);
    }

    let body_data = &response[headers_end..];
    let written = write_fd(fd, body_data);
    close(fd);

    if written < 0 {
        print("wget: Failed to write file\n");
        exit(1);
    }

    print("wget: Saved ");
    print_num(body_data.len());
    print(" bytes to ");
    print(&output_file);
    print("\n");

    exit(0);
}

struct ParsedUrl<'a> {
    host: &'a str,
    port: u16,
    path: &'a str,
}

fn parse_url(url: &str) -> Option<ParsedUrl> {
    // Remove http:// prefix
    let rest = url.strip_prefix("http://")?;

    // Find path separator
    let (host_port, path) = match rest.find('/') {
        Some(pos) => (&rest[..pos], &rest[pos..]),
        None => (rest, "/"),
    };

    // Parse host:port
    let (host, port) = match host_port.rfind(':') {
        Some(pos) => {
            let h = &host_port[..pos];
            let p = host_port[pos + 1..].parse::<u16>().ok()?;
            (h, p)
        }
        None => (host_port, 80),
    };

    Some(ParsedUrl { host, port, path })
}

fn parse_response(data: &[u8]) -> Option<(u16, usize, &[u8])> {
    // Find headers end
    let headers_end = find_headers_end(data)?;
    
    // Parse status line
    let header_str = core::str::from_utf8(&data[..headers_end]).ok()?;
    let first_line = header_str.lines().next()?;
    
    // Parse "HTTP/1.x STATUS MESSAGE"
    let mut parts = first_line.split_whitespace();
    let _version = parts.next()?;
    let status: u16 = parts.next()?.parse().ok()?;

    Some((status, headers_end, &data[headers_end..]))
}

fn find_headers_end(data: &[u8]) -> Option<usize> {
    // Look for \r\n\r\n
    for i in 0..data.len().saturating_sub(3) {
        if &data[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 4);
        }
    }
    None
}

fn extract_filename(path: &str) -> String {
    // Get last path component
    let name = path.rsplit('/').next().unwrap_or("index.html");
    if name.is_empty() {
        String::from("index.html")
    } else {
        String::from(name)
    }
}

fn print_num(n: usize) {
    let mut buf = [0u8; 20];
    let mut i = 19;
    let mut v = n;

    if v == 0 {
        print("0");
        return;
    }

    while v > 0 && i > 0 {
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        i -= 1;
    }

    if let Ok(s) = core::str::from_utf8(&buf[i + 1..]) {
        print(s);
    }
}

fn print_ip(ip: [u8; 4]) {
    print_num(ip[0] as usize);
    print(".");
    print_num(ip[1] as usize);
    print(".");
    print_num(ip[2] as usize);
    print(".");
    print_num(ip[3] as usize);
}

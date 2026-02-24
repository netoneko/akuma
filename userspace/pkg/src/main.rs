#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::*;
use libakuma::net::{TcpStream, ErrorKind};

#[no_mangle]
pub extern "C" fn main() {
    let args: Vec<String> = args().map(String::from).collect();
    cmd_pkg(&args);
}

// ============================================================================
// Core `pkg install` Logic (Moved from paws)
// ============================================================================

fn cmd_pkg(args: &[String]) {
    if args.len() < 3 || args[1] != "install" {
        println("Usage: pkg install <package>");
        return;
    }
    
    let server = "10.0.2.2:8000";
    
    for package in &args[2..] {
        println(format!("Installing {}...", package).as_str());
        
        // 1. Try to download as a binary
        let bin_url = format!("http://{}/bin/{}", server, package);
        let bin_dest = format!("/bin/{}", package);
        
        if download_file(&bin_url, &bin_dest) == 0 {
             println(format!("Successfully installed binary to {}", bin_dest).as_str());
             continue;
        }

        // 2. Fallback to downloading as an archive
        let archive_url_gz = format!("http://{}/archives/{}.tar.gz", server, package);
        let archive_dest_gz = format!("/tmp/{}.tar.gz", package);
        let archive_url_raw = format!("http://{}/archives/{}.tar", server, package);
        let archive_dest_raw = format!("/tmp/{}.tar", package);
        
        let (_archive_url, archive_dest, is_gz) = if download_file(&archive_url_gz, &archive_dest_gz) == 0 {
            (archive_url_gz, archive_dest_gz, true)
        } else if download_file(&archive_url_raw, &archive_dest_raw) == 0 {
            (archive_url_raw, archive_dest_raw, false)
        } else {
            println(format!("Failed to find package {} as binary or archive.", package).as_str());
            continue;
        };

        println(format!("Extracting {} to /...", archive_dest).as_str());
        let mut tar_args = Vec::new();
        tar_args.push(String::from("tar"));
        tar_args.push(String::from(if is_gz { "-xzvf" } else { "-xvf" }));
        tar_args.push(archive_dest.clone());
        tar_args.push(String::from("-C"));
        tar_args.push(String::from("/"));

        if execute_external_with_status(&tar_args) == 0 {
            println("Successfully extracted archive.");
            let _ = unlink(&archive_dest);
        } else {
            println("Failed to extract archive.");
        }
    }
}

// ============================================================================
// `download_file` and HTTP Helpers (Moved from paws)
// ============================================================================

fn download_file(url: &str, dest_path: &str) -> i32 {
    print("pkg: downloading ");
    print(url);
    print(" to ");
    println(dest_path);

    let parsed = match parse_url(url) {
        Some(p) => p,
        None => {
            println("pkg: invalid URL format");
            return -1;
        }
    };

    let ip = match libakuma::net::resolve(parsed.host) {
        Ok(ip) => ip,
        Err(_) => {
            println("pkg: DNS resolution failed");
            return -1;
        }
    };

    let addr_str = format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], parsed.port);
    print("pkg: connecting to ");
    println(&addr_str);

    let stream = match TcpStream::connect(&addr_str) {
        Ok(s) => s,
        Err(e) => {
            print("pkg: connection failed: ");
            print(&format!("{:?}\n", e));
            return -1;
        }
    };

    let request = format!(
        "GET {} HTTP/1.0\r\n\
         Host: {}\r\n\
         User-Agent: pkg/1.0 (Akuma)\r\n\
         Connection: close\r\n\
         \r\n",
        parsed.path,
        parsed.host
    );

    if let Err(e) = stream.write_all(request.as_bytes()) {
        print("pkg: failed to send request: ");
        print(&format!("{:?}\n", e));
        return -1;
    }

    let mut response_buf = Vec::new();
    let mut buf = [0u8; 4096];
    let mut headers_end = None;
    
    while headers_end.is_none() {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                response_buf.extend_from_slice(&buf[..n]);
                headers_end = find_headers_end(&response_buf);
            }
            Err(e) => {
                if e.kind == ErrorKind::WouldBlock || e.kind == ErrorKind::TimedOut {
                    continue;
                }
                print("pkg: header read error: ");
                print(&format!("{:?}\n", e));
                return -1;
            }
        }
    }

    let end_pos = match headers_end {
        Some(pos) => pos,
        None => {
            println("pkg: failed to find HTTP headers");
            return -1;
        }
    };

    let header_str = core::str::from_utf8(&response_buf[..end_pos]).unwrap_or("");
    let mut status = 0;
    let mut content_length = None;

    for (i, line) in header_str.lines().enumerate() {
        if i == 0 {
            let mut parts = line.split_whitespace();
            parts.next();
            status = parts.next().and_then(|s| s.parse::<u16>().ok()).unwrap_or(0);
        } else if line.to_lowercase().starts_with("content-length:") {
            content_length = line["content-length:".len()..].trim().parse::<usize>().ok();
        }
    }

    if status != 200 {
        print("pkg: server returned HTTP ");
        print_dec(status as usize);
        println("");
        return -1;
    }

    let fd = open(dest_path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd < 0 {
        print("pkg: failed to open destination: ");
        print_dec((-fd) as usize);
        println("");
        return -1;
    }

    let initial_body = &response_buf[end_pos..];
    if !initial_body.is_empty() {
        write_fd(fd, initial_body);
    }

    let mut total_downloaded = initial_body.len();
    
    loop {
        if let Some(len) = content_length {
            if total_downloaded >= len {
                break;
            }
        }

        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let res = write_fd(fd, &buf[..n]);
                if res < 0 {
                    print("pkg: failed to write to destination file: ");
                    print_dec((-res) as usize);
                    println("");
                    close(fd);
                    return -1;
                }
                total_downloaded += n;
            }
            Err(e) => {
                if e.kind == ErrorKind::WouldBlock || e.kind == ErrorKind::TimedOut {
                    continue;
                }
                print("pkg: body read error: ");
                print(&format!("{:?}\n", e));
                close(fd);
                return -1;
            }
        }
    }

    close(fd);
    print("pkg: received ");
    print_dec(total_downloaded);
    println(" bytes");

    let verify_fd = open(dest_path, open_flags::O_RDONLY);
    if verify_fd < 0 {
        print("pkg: verify failed - file not found after closing: ");
        print_dec((-verify_fd) as usize);
        println("");
        return -1;
    }
    close(verify_fd);

    0
}

trait ToLowercaseExt {
    fn to_lowercase(&self) -> String;
}

impl ToLowercaseExt for &str {
    fn to_lowercase(&self) -> String {
        let mut s = String::with_capacity(self.len());
        for c in self.chars() {
            for lc in c.to_lowercase() {
                s.push(lc);
            }
        }
        s
    }
}

struct ParsedUrl<'a> {
    host: &'a str,
    port: u16,
    path: &'a str,
}

fn parse_url(url: &str) -> Option<ParsedUrl> {
    let rest = url.strip_prefix("http://")?;
    let (host_port, path) = match rest.find('/') {
        Some(pos) => (&rest[..pos], &rest[pos..]),
        None => (rest, "/"),
    };
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

fn find_headers_end(data: &[u8]) -> Option<usize> {
    for i in 0..data.len().saturating_sub(3) {
        if &data[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 4);
        }
    }
    None
}

// ============================================================================
// External Command Execution Helpers (Moved from paws)
// ============================================================================

fn find_bin(name: &str) -> String {
    if name.starts_with('/') || name.starts_with("./") {
        String::from(name)
    } else {
        format!("/bin/{}", name)
    }
}

fn execute_external_with_status(args: &[String]) -> i32 {
    let path = find_bin(&args[0]);
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    
    print("pkg: executing ");
    print(&path);
    for arg in arg_refs.iter().skip(1) {
        print(" ");
        print(arg);
    }
    println("");

    if let Some(res) = spawn(&path, Some(&arg_refs)) {
        let status = stream_output(res.stdout_fd, res.pid);
        print("pkg: process ");
        print(&path);
        print(" exited with status ");
        print_dec(status as usize);
        println("");
        return status;
    } else {
        print("pkg: command not found: ");
        println(&args[0]);
        return -1;
    }
}

fn stream_output(stdout_fd: u32, pid: u32) -> i32 {
    let mut buf = [0u8; 1024];
    loop {
        let n = read_fd(stdout_fd as i32, &mut buf);
        if n > 0 { write(fd::STDOUT, &buf[..n as usize]); }
        
        if let Some((_, exit_code)) = waitpid(pid) {
            // Drain any remaining output
            loop {
                let n = read_fd(stdout_fd as i32, &mut buf);
                if n <= 0 { break; }
                write(fd::STDOUT, &buf[..n as usize]);
            }
            return exit_code;
        }
        sleep_ms(5);
    }
}

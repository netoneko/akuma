//! Network Commands (Userspace Port)

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use embedded_io_async::Write;
use libakuma::*;
use libakuma::net::{TcpStream, resolve};
use crate::shell::{Command, ShellContext, ShellError, VecWriter};

// ============================================================================
// URL Parsing Helper
// ============================================================================

struct ParsedUrl {
    host: String,
    port: u16,
    path: String,
}

fn parse_url(url: &str) -> Option<ParsedUrl> {
    let rest = if let Some(rest) = url.strip_prefix("http://") {
        rest
    } else if let Some(rest) = url.strip_prefix("https://") {
        rest
    } else {
        url
    };

    let (host_port, path) = if let Some(pos) = rest.find('/') {
        (&rest[..pos], &rest[pos..])
    } else {
        (rest, "/")
    };

    let (host, port) = if let Some(pos) = host_port.rfind(':') {
        let port_str = &host_port[pos + 1..];
        if let Ok(p) = port_str.parse::<u16>() {
            (&host_port[..pos], p)
        } else {
            (host_port, 80)
        }
    } else {
        (host_port, 80)
    };

    if host.is_empty() {
        return None;
    }

    Some(ParsedUrl {
        host: String::from(host),
        port,
        path: String::from(path),
    })
}

// ============================================================================
// HTTP GET Helper
// ============================================================================

async fn http_get(url: &ParsedUrl) -> Result<Vec<u8>, String> {
    // Resolve hostname
    let ip = resolve(&url.host).map_err(|e| format!("DNS resolution failed: {:?}", e))?;
    let addr_str = format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], url.port);

    // Connect
    let stream = TcpStream::connect(&addr_str).map_err(|e| format!("TCP connection failed: {:?}", e))?;

    // Send HTTP/1.0 GET request
    let request = format!(
        "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: akuma-sshd/1.0\r\nConnection: close\r\n\r\n",
        url.path, url.host
    );
    stream.write_all(request.as_bytes()).map_err(|e| format!("Send failed: {:?}", e))?;

    // Read response
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(e) => {
                if e.kind == libakuma::net::ErrorKind::WouldBlock || e.kind == libakuma::net::ErrorKind::TimedOut {
                    sleep_ms(10);
                    continue;
                }
                return Err(format!("TCP read error: {:?}", e));
            }
        }
    }

    // Parse HTTP response
    let pos = match response.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(p) => p,
        None => return Err(String::from("Malformed HTTP response")),
    };

    Ok(response[pos + 4..].to_vec())
}

// ============================================================================
// Curl Command
// ============================================================================

pub struct CurlCommand;
impl Command for CurlCommand {
    fn name(&self) -> &'static str { "curl" }
    fn description(&self) -> &'static str { "HTTP GET request" }
    fn execute<'a>(&'a self, args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, _ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let url_str = core::str::from_utf8(args).unwrap_or("").trim();
            if let Some(url) = parse_url(url_str) {
                match http_get(&url).await {
                    Ok(body) => { let _ = stdout.write(&body).await; }
                    Err(e) => { let _ = stdout.write(format!("Error: {}\r\n", e).as_bytes()).await; }
                }
            } else { let _ = stdout.write(b"Usage: curl <url>\r\n").await; }
            Ok(())
        })
    }
}
pub static CURL_CMD: CurlCommand = CurlCommand;

// ============================================================================
// Nslookup Command
// ============================================================================

pub struct NslookupCommand;
impl Command for NslookupCommand {
    fn name(&self) -> &'static str { "nslookup" }
    fn description(&self) -> &'static str { "DNS lookup" }
    fn execute<'a>(&'a self, args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, _ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let hostname = core::str::from_utf8(args).unwrap_or("").trim();
            if !hostname.is_empty() {
                match resolve(hostname) {
                    Ok(ip) => { let _ = stdout.write(format!("Name: {}\r\nAddress: {}.{}.{}.{}\r\n", hostname, ip[0], ip[1], ip[2], ip[3]).as_bytes()).await; }
                    Err(e) => { let _ = stdout.write(format!("DNS lookup failed: {:?}\r\n", e).as_bytes()).await; }
                }
            } else { let _ = stdout.write(b"Usage: nslookup <hostname>\r\n").await; }
            Ok(())
        })
    }
}
pub static NSLOOKUP_CMD: NslookupCommand = NslookupCommand;

// ============================================================================
// Pkg Command
// ============================================================================

pub struct PkgCommand;
impl Command for PkgCommand {
    fn name(&self) -> &'static str { "pkg" }
    fn description(&self) -> &'static str { "Package manager" }
    fn execute<'a>(&'a self, args: &'a [u8], _stdin: Option<&'a [u8]>, stdout: &'a mut VecWriter, _ctx: &'a mut ShellContext) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let args_str = core::str::from_utf8(args).unwrap_or("").trim();
            if let Some(rest) = args_str.strip_prefix("install") {
                let pkg = rest.trim();
                if !pkg.is_empty() {
                    let _ = stdout.write(format!("Installing {}...\r\n", pkg).as_bytes()).await;
                    let url_str = format!("http://10.0.2.2:8000/bin/{}", pkg);
                    if let Some(url) = parse_url(&url_str) {
                        match http_get(&url).await {
                            Ok(body) => {
                                let dest = format!("/bin/{}", pkg);
                                let fd = open(&dest, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
                                if fd >= 0 {
                                    write_fd(fd, &body);
                                    close(fd);
                                    let _ = stdout.write(b"Successfully installed.\r\n").await;
                                } else { let _ = stdout.write(b"Failed to open destination file.\r\n").await; }
                            }
                            Err(e) => { let _ = stdout.write(format!("Download failed: {}\r\n", e).as_bytes()).await; }
                        }
                    }
                } else { let _ = stdout.write(b"Usage: pkg install <name>\r\n").await; }
            } else { let _ = stdout.write(b"Usage: pkg install <name>\r\n").await; }
            Ok(())
        })
    }
}
pub static PKG_CMD: PkgCommand = PkgCommand;

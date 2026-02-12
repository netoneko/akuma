//! Network Commands
//!
//! Commands for network operations: curl, nslookup, pkg

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use embedded_io_async::Write;

use crate::shell::{Command, ShellContext, ShellError, VecWriter};
use crate::smoltcp_net;

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
        // HTTPS not supported in kernel shell, treat as HTTP
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

/// Perform an HTTP GET request and return the response body.
/// Resolves DNS, connects via TCP, sends request, reads response.
async fn http_get(url: &ParsedUrl) -> Result<Vec<u8>, &'static str> {
    // Resolve hostname
    let ip = smoltcp_net::dns_query(&url.host)
        .map_err(|_| "DNS resolution failed")?;

    // Connect
    let (mut stream, handle) = smoltcp_net::tcp_connect(
        smoltcp::wire::IpAddress::Ipv4(ip),
        url.port,
    ).await.map_err(|_| "TCP connection failed")?;

    // Send HTTP/1.0 GET request (HTTP/1.0 closes connection after response)
    let request = format!(
        "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: akuma/1.0\r\nConnection: close\r\n\r\n",
        url.path, url.host
    );
    let _ = stream.write(request.as_bytes()).await.map_err(|_| "Send failed")?;
    let _ = stream.flush().await;

    // Read the full response.
    // Poll the network stack between reads to ensure TCP ACKs are sent
    // promptly. Without this, ACKs are only sent when the receive buffer
    // is fully drained (Pending), creating a stop-and-go pattern that can
    // cause connection resets during large transfers through QEMU slirp.
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    let mut had_error = false;
    loop {
        // Drive the network stack so ACKs and window updates are sent
        // between reads, not just when the buffer is empty
        smoltcp_net::poll();
        match embedded_io_async::Read::read(&mut stream, &mut buf).await {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(_) => {
                had_error = true;
                break;
            }
        }
    }

    smoltcp_net::socket_close(handle);

    if response.is_empty() {
        return Err(if had_error { "TCP read error" } else { "Empty response" });
    }

    // Parse HTTP response: find end of headers (\r\n\r\n)
    let pos = match response.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(p) => p,
        None => return Err("Malformed HTTP response (no header terminator)"),
    };

    let header_bytes = &response[..pos];
    // Check for successful status
    if let Ok(header_str) = core::str::from_utf8(header_bytes) {
        if let Some(status_line) = header_str.lines().next() {
            if !status_line.contains("200") && !status_line.contains("301") && !status_line.contains("302") {
                return Err("HTTP error response");
            }
        }
    }

    let body = response[pos + 4..].to_vec();

    // Verify against Content-Length if present (case-insensitive header match)
    if let Ok(header_str) = core::str::from_utf8(header_bytes) {
        for line in header_str.lines() {
            // HTTP headers are case-insensitive
            if line.len() > 15 && line.as_bytes()[..15].eq_ignore_ascii_case(b"content-length:") {
                if let Ok(expected) = line[15..].trim().parse::<usize>() {
                    if body.len() < expected {
                        // Download was truncated - don't silently return partial data
                        return Err("Download incomplete (connection lost)");
                    }
                }
            }
        }
    }

    if had_error && body.is_empty() {
        return Err("TCP read error (no body received)");
    }

    Ok(body)
}

// ============================================================================
// Curl Command
// ============================================================================

pub struct CurlCommand;

impl Command for CurlCommand {
    fn name(&self) -> &'static str { "curl" }
    fn description(&self) -> &'static str { "HTTP GET request" }
    fn usage(&self) -> &'static str { "curl <url>" }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        _ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let args_str = core::str::from_utf8(args).unwrap_or("").trim();
            if args_str.is_empty() {
                let _ = stdout.write(b"Usage: curl <url>\r\n").await;
                return Ok(());
            }

            let url = match parse_url(args_str) {
                Some(u) => u,
                None => {
                    let _ = stdout.write(b"Error: Invalid URL\r\n").await;
                    return Ok(());
                }
            };

            let msg = format!("Connecting to {}:{}...\r\n", url.host, url.port);
            let _ = stdout.write(msg.as_bytes()).await;

            match http_get(&url).await {
                Ok(body) => {
                    let _ = stdout.write(&body).await;
                    if !body.ends_with(b"\n") {
                        let _ = stdout.write(b"\r\n").await;
                    }
                }
                Err(e) => {
                    let msg = format!("Error: {}\r\n", e);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
            }
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
    fn usage(&self) -> &'static str { "nslookup <hostname>" }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        _ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let hostname = core::str::from_utf8(args).unwrap_or("").trim();
            if hostname.is_empty() {
                let _ = stdout.write(b"Usage: nslookup <hostname>\r\n").await;
                return Ok(());
            }

            let _ = stdout.write(b"Server: 10.0.2.3\r\n").await;
            let msg = format!("Name:   {}\r\n", hostname);
            let _ = stdout.write(msg.as_bytes()).await;

            match smoltcp_net::dns_query(hostname) {
                Ok(ip) => {
                    let msg = format!("Address: {}\r\n", ip);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
                Err(e) => {
                    let msg = format!("** DNS lookup failed: {:?}\r\n", e);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
            }
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
    fn usage(&self) -> &'static str { "pkg install <package>" }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        _ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let args_str = core::str::from_utf8(args).unwrap_or("").trim();

            // Parse "install <package>"
            let package = if let Some(rest) = args_str.strip_prefix("install") {
                rest.trim()
            } else {
                let _ = stdout.write(b"Usage: pkg install <package>\r\n\r\nExamples:\r\n  pkg install stdcheck\r\n  pkg install echo2\r\n").await;
                return Ok(());
            };

            if package.is_empty() {
                let _ = stdout.write(b"Error: No package name specified\r\nUsage: pkg install <package>\r\n").await;
                return Ok(());
            }

            // Download from host HTTP server (10.0.2.2:8000)
            let url_str = format!(
                "http://10.0.2.2:8000/target/aarch64-unknown-none/release/{}",
                package
            );
            let url = match parse_url(&url_str) {
                Some(u) => u,
                None => {
                    let _ = stdout.write(b"Error: Failed to construct URL\r\n").await;
                    return Ok(());
                }
            };

            let msg = format!("pkg: downloading {}...\r\n", package);
            let _ = stdout.write(msg.as_bytes()).await;

            match http_get(&url).await {
                Ok(body) => {
                    if body.is_empty() {
                        let _ = stdout.write(b"Error: Empty response (package not found?)\r\n").await;
                        return Ok(());
                    }

                    let size_msg = format!("pkg: downloaded {} bytes\r\n", body.len());
                    let _ = stdout.write(size_msg.as_bytes()).await;

                    // Check filesystem capacity before writing
                    if let Ok(stats) = crate::fs::stats() {
                        let free_bytes = stats.free_bytes();
                        let block_size = stats.cluster_size;
                        let blocks_needed = (body.len() as u64 + block_size as u64 - 1) / block_size as u64;
                        let diag = format!(
                            "pkg: fs: block_size={}, free={} bytes ({} blocks), need ~{} blocks\r\n",
                            block_size, free_bytes, stats.free_clusters, blocks_needed
                        );
                        let _ = stdout.write(diag.as_bytes()).await;

                        if (body.len() as u64) > free_bytes {
                            let _ = stdout.write(b"Error: Not enough disk space\r\n").await;
                            return Ok(());
                        }
                    }

                    // Check disk device capacity
                    if let Some(disk_cap) = crate::block::capacity() {
                        let diag = format!("pkg: disk capacity={} bytes\r\n", disk_cap);
                        let _ = stdout.write(diag.as_bytes()).await;
                    }

                    // Ensure /bin directory exists
                    if crate::fs::create_dir("/bin").is_err() {
                        // Ignore error - directory may already exist
                    }

                    // Write to /bin/<package>
                    let dest = format!("/bin/{}", package);
                    match crate::fs::write_file(&dest, &body) {
                        Ok(()) => {
                            let msg = format!(
                                "pkg: installed {} ({} bytes) -> {}\r\n",
                                package,
                                body.len(),
                                dest
                            );
                            let _ = stdout.write(msg.as_bytes()).await;
                        }
                        Err(e) => {
                            let msg = format!("Error: Failed to write to /bin/ ({} bytes): {}\r\n", body.len(), e);
                            let _ = stdout.write(msg.as_bytes()).await;
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("Error downloading {}: {}\r\n", package, e);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
            }
            Ok(())
        })
    }
}

pub static PKG_CMD: PkgCommand = PkgCommand;

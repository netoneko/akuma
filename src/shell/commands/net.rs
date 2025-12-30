//! Network Commands
//!
//! Commands for network operations: curl, nslookup

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use crate::async_net;
use crate::dns;
use crate::shell::{Command, ShellError};

// ============================================================================
// Curl Command
// ============================================================================

/// Curl command - HTTP GET request
pub struct CurlCommand;

impl Command for CurlCommand {
    fn name(&self) -> &'static str {
        "curl"
    }
    fn description(&self) -> &'static str {
        "HTTP GET request"
    }
    fn usage(&self) -> &'static str {
        "curl <url>"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ShellError>> + 'a>> {
        Box::pin(async move {
            let mut response = Vec::new();

            if args.is_empty() {
                response.extend_from_slice(b"Usage: curl <url>\r\n");
                response.extend_from_slice(b"Example: curl http://10.0.2.2:8080/\r\n");
                return Ok(response);
            }

            let url = match core::str::from_utf8(args) {
                Ok(s) => s.trim(),
                Err(_) => {
                    response.extend_from_slice(b"Error: Invalid URL\r\n");
                    return Ok(response);
                }
            };

            match http_get(url).await {
                Ok(body) => {
                    for line in body.split('\n') {
                        response.extend_from_slice(line.as_bytes());
                        response.extend_from_slice(b"\r\n");
                    }
                }
                Err(e) => {
                    let msg = format!("Error: {}\r\n", e);
                    response.extend_from_slice(msg.as_bytes());
                }
            }
            Ok(response)
        })
    }
}

/// Static instance
pub static CURL_CMD: CurlCommand = CurlCommand;

// ============================================================================
// Nslookup Command
// ============================================================================

/// Nslookup command - DNS lookup with timing
pub struct NslookupCommand;

impl Command for NslookupCommand {
    fn name(&self) -> &'static str {
        "nslookup"
    }
    fn description(&self) -> &'static str {
        "DNS lookup with timing"
    }
    fn usage(&self) -> &'static str {
        "nslookup <hostname>"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ShellError>> + 'a>> {
        Box::pin(async move {
            let mut response = Vec::new();

            if args.is_empty() {
                response.extend_from_slice(b"Usage: nslookup <hostname>\r\n");
                response.extend_from_slice(b"Example: nslookup example.com\r\n");
                return Ok(response);
            }

            let host = match core::str::from_utf8(args) {
                Ok(s) => s.trim(),
                Err(_) => {
                    response.extend_from_slice(b"Error: Invalid hostname\r\n");
                    return Ok(response);
                }
            };

            let stack = match async_net::get_global_stack() {
                Some(s) => s,
                None => {
                    response.extend_from_slice(b"Error: Network not initialized\r\n");
                    return Ok(response);
                }
            };

            // Show DNS server if available
            if let Some(dns_server) = dns::get_dns_server(&stack) {
                let msg = format!(
                    "Server: {}.{}.{}.{}\r\n",
                    dns_server.octets()[0],
                    dns_server.octets()[1],
                    dns_server.octets()[2],
                    dns_server.octets()[3]
                );
                response.extend_from_slice(msg.as_bytes());
            }

            // Perform DNS resolution with timing
            match dns::resolve_host(host, &stack).await {
                Ok((ip, duration)) => {
                    let ip_str = match ip {
                        embassy_net::IpAddress::Ipv4(v4) => format!(
                            "{}.{}.{}.{}",
                            v4.octets()[0],
                            v4.octets()[1],
                            v4.octets()[2],
                            v4.octets()[3]
                        ),
                    };
                    let msg = format!("Address: {}\r\n", ip_str);
                    response.extend_from_slice(msg.as_bytes());

                    let time_ms = duration.as_millis();
                    let msg = format!("Time: {}ms\r\n", time_ms);
                    response.extend_from_slice(msg.as_bytes());
                }
                Err(e) => {
                    let msg = format!("Error: {}\r\n", e.as_str());
                    response.extend_from_slice(msg.as_bytes());
                }
            }

            Ok(response)
        })
    }
}

/// Static instance
pub static NSLOOKUP_CMD: NslookupCommand = NslookupCommand;

// ============================================================================
// HTTP Helper
// ============================================================================

/// Perform an HTTP GET request (public for legacy API)
pub async fn http_get_legacy(url: &str) -> Result<String, &'static str> {
    http_get(url).await
}

/// Perform an HTTP GET request
async fn http_get(url: &str) -> Result<String, &'static str> {
    use embassy_net::tcp::TcpSocket;
    use embassy_net::IpEndpoint;
    use embassy_time::Duration;
    use embedded_io_async::Write as AsyncWrite;

    // Get main stack for DNS resolution
    let main_stack = async_net::get_global_stack().ok_or("Network not initialized")?;

    let url = url
        .strip_prefix("http://")
        .ok_or("Only http:// URLs supported")?;

    let (host_port, path) = url.split_once('/').unwrap_or((url, ""));
    let path = format!("/{}", path);

    let (host, port) = if let Some((h, p)) = host_port.split_once(':') {
        (h, p.parse::<u16>().map_err(|_| "Invalid port")?)
    } else {
        (host_port, 80u16)
    };

    // Resolve hostname to IP
    let (ip, _duration) = dns::resolve_host(host, &main_stack)
        .await
        .map_err(|e| e.as_str())?;

    // Get the appropriate stack for this IP (loopback for 127.x.x.x)
    let stack = dns::get_stack_for_ip(ip).ok_or("Network stack not available")?;

    let mut rx_buf = [0u8; 2048];
    let mut tx_buf = [0u8; 1024];
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
    socket.set_timeout(Some(Duration::from_secs(10)));

    let endpoint = IpEndpoint::new(ip, port);
    socket
        .connect(endpoint)
        .await
        .map_err(|_| "Connection failed")?;

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: akuma-curl/1.0\r\n\r\n",
        path, host
    );
    socket
        .write_all(request.as_bytes())
        .await
        .map_err(|_| "Write failed")?;

    let mut response_data = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        match socket.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => response_data.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }

    socket.close();

    let response_str = String::from_utf8(response_data).map_err(|_| "Invalid UTF-8 response")?;

    if let Some(body_start) = response_str.find("\r\n\r\n") {
        Ok(response_str[body_start + 4..].to_string())
    } else {
        Ok(response_str)
    }
}

use alloc::string::ToString;

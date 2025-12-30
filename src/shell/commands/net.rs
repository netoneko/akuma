//! Network Commands
//!
//! Commands for network operations: curl, nslookup
//! Supports both HTTP and HTTPS (TLS 1.3) connections.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use embedded_io_async::Write;

use crate::async_net;
use crate::dns;
use crate::shell::{Command, ShellError, VecWriter};
use crate::tls::{TlsOptions, TlsStream, TLS_RECORD_SIZE};

// ============================================================================
// Curl Options
// ============================================================================

/// Parsed curl command options
#[derive(Debug, Clone, Default)]
struct CurlOpts<'a> {
    /// Skip certificate verification (-k, --insecure)
    insecure: bool,
    /// Verbose output (-v, --verbose)
    verbose: bool,
    /// The URL to fetch
    url: Option<&'a str>,
}

impl<'a> CurlOpts<'a> {
    /// Parse command-line arguments
    fn parse(args: &'a str) -> Self {
        let mut opts = CurlOpts::default();

        for arg in args.split_whitespace() {
            match arg {
                "-k" | "--insecure" => opts.insecure = true,
                "-v" | "--verbose" => opts.verbose = true,
                s if !s.starts_with('-') => opts.url = Some(s),
                _ => {} // Ignore unknown options
            }
        }

        opts
    }
}

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
        "HTTP/HTTPS GET request"
    }
    fn usage(&self) -> &'static str {
        "curl [-k] [-v] <url>"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let args_str = match core::str::from_utf8(args) {
                Ok(s) => s.trim(),
                Err(_) => {
                    let _ = stdout.write(b"Error: Invalid arguments\r\n").await;
                    return Ok(());
                }
            };

            if args_str.is_empty() {
                let _ = stdout.write(b"Usage: curl [-k] [-v] <url>\r\n").await;
                let _ = stdout.write(b"\r\n").await;
                let _ = stdout.write(b"Options:\r\n").await;
                let _ = stdout
                    .write(b"  -k, --insecure  Skip certificate verification\r\n")
                    .await;
                let _ = stdout
                    .write(b"  -v, --verbose   Show detailed connection info\r\n")
                    .await;
                let _ = stdout.write(b"\r\n").await;
                let _ = stdout.write(b"Examples:\r\n").await;
                let _ = stdout
                    .write(b"  curl http://10.0.2.2:8080/\r\n")
                    .await;
                let _ = stdout
                    .write(b"  curl https://example.com/\r\n")
                    .await;
                let _ = stdout
                    .write(b"  curl -k https://self-signed.example.com/\r\n")
                    .await;
                let _ = stdout
                    .write(b"  curl -v https://example.com/\r\n")
                    .await;
                return Ok(());
            }

            let opts = CurlOpts::parse(args_str);

            let url = match opts.url {
                Some(u) => u,
                None => {
                    let _ = stdout.write(b"Error: No URL specified\r\n").await;
                    return Ok(());
                }
            };

            // Build TLS options
            let tls_opts = TlsOptions {
                insecure: opts.insecure,
                verbose: opts.verbose,
            };

            match http_get_with_options(url, tls_opts, stdout).await {
                Ok(body) => {
                    for line in body.split('\n') {
                        let _ = stdout.write(line.as_bytes()).await;
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
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            if args.is_empty() {
                let _ = stdout.write(b"Usage: nslookup <hostname>\r\n").await;
                let _ = stdout.write(b"Example: nslookup example.com\r\n").await;
                return Ok(());
            }

            let host = match core::str::from_utf8(args) {
                Ok(s) => s.trim(),
                Err(_) => {
                    let _ = stdout.write(b"Error: Invalid hostname\r\n").await;
                    return Ok(());
                }
            };

            let stack = match async_net::get_global_stack() {
                Some(s) => s,
                None => {
                    let _ = stdout.write(b"Error: Network not initialized\r\n").await;
                    return Ok(());
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
                let _ = stdout.write(msg.as_bytes()).await;
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
                    let _ = stdout.write(msg.as_bytes()).await;

                    let time_ms = duration.as_millis();
                    let msg = format!("Time: {}ms\r\n", time_ms);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
                Err(e) => {
                    let msg = format!("Error: {}\r\n", e.as_str());
                    let _ = stdout.write(msg.as_bytes()).await;
                }
            }

            Ok(())
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
    http_get_with_options(url, TlsOptions::default(), &mut DummyWriter).await
}

/// Dummy writer for legacy API that doesn't need verbose output
struct DummyWriter;

impl embedded_io_async::ErrorType for DummyWriter {
    type Error = core::convert::Infallible;
}

impl embedded_io_async::Write for DummyWriter {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        Ok(buf.len())
    }
    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Perform an HTTP/HTTPS GET request with options
///
/// Supports both `http://` and `https://` URLs.
/// HTTPS uses TLS 1.3 with optional certificate verification.
async fn http_get_with_options<W: embedded_io_async::Write>(
    url: &str,
    tls_opts: TlsOptions,
    stdout: &mut W,
) -> Result<String, &'static str> {
    use embassy_net::IpEndpoint;
    use embassy_net::tcp::TcpSocket;
    use embassy_time::Duration;

    let verbose = tls_opts.verbose;

    // Get main stack for DNS resolution
    let main_stack = async_net::get_global_stack().ok_or("Network not initialized")?;

    // Detect scheme (http vs https)
    let (use_tls, url_rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err("URL must start with http:// or https://");
    };

    let (host_port, path) = url_rest.split_once('/').unwrap_or((url_rest, ""));
    let path = format!("/{}", path);

    let default_port = if use_tls { 443 } else { 80 };
    let (host, port) = if let Some((h, p)) = host_port.split_once(':') {
        (h, p.parse::<u16>().map_err(|_| "Invalid port")?)
    } else {
        (host_port, default_port)
    };

    if verbose {
        let msg = format!("* Connecting to {}:{}\r\n", host, port);
        let _ = stdout.write(msg.as_bytes()).await;
    }

    // Resolve hostname to IP
    if verbose {
        let msg = format!("* Resolving hostname: {}\r\n", host);
        let _ = stdout.write(msg.as_bytes()).await;
    }

    let (ip, duration) = dns::resolve_host(host, &main_stack)
        .await
        .map_err(|e| e.as_str())?;

    if verbose {
        let ip_str = match ip {
            embassy_net::IpAddress::Ipv4(v4) => format!(
                "{}.{}.{}.{}",
                v4.octets()[0],
                v4.octets()[1],
                v4.octets()[2],
                v4.octets()[3]
            ),
        };
        let msg = format!("* Resolved to {} ({}ms)\r\n", ip_str, duration.as_millis());
        let _ = stdout.write(msg.as_bytes()).await;
    }

    // Get the appropriate stack for this IP (loopback for 127.x.x.x)
    let stack = dns::get_stack_for_ip(ip).ok_or("Network stack not available")?;

    // Use larger buffers for TLS
    let mut rx_buf = [0u8; 4096];
    let mut tx_buf = [0u8; 4096];
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
    socket.set_timeout(Some(Duration::from_secs(30)));

    let endpoint = IpEndpoint::new(ip, port);

    if verbose {
        let _ = stdout.write(b"* Establishing TCP connection...\r\n").await;
    }

    socket
        .connect(endpoint)
        .await
        .map_err(|_| "Connection failed")?;

    if verbose {
        let _ = stdout.write(b"* TCP connection established\r\n").await;
    }

    if use_tls {
        // HTTPS with TLS
        if verbose {
            let _ = stdout.write(b"* Starting TLS handshake...\r\n").await;
        }

        let mut tls_rx = [0u8; TLS_RECORD_SIZE];
        let mut tls_tx = [0u8; TLS_RECORD_SIZE];

        let mut tls = match TlsStream::connect_with_options(
            socket,
            host,
            &mut tls_rx,
            &mut tls_tx,
            tls_opts,
        )
        .await
        {
            Ok(tls) => tls,
            Err(e) => {
                if verbose {
                    let msg = format!("* TLS error: {:?}\r\n", e);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
                return Err("TLS handshake failed");
            }
        };

        if verbose {
            let _ = stdout.write(b"* TLS handshake complete\r\n").await;
        }

        let result = http_request_on_stream(&mut tls, host, &path, verbose, stdout).await;

        // Try to close TLS gracefully (ignore errors)
        let _ = tls.close().await;

        if verbose {
            let _ = stdout.write(b"* Connection closed\r\n").await;
        }

        result
    } else {
        // Plain HTTP
        let result = http_request_on_stream(&mut socket, host, &path, verbose, stdout).await;
        socket.close();

        if verbose {
            let _ = stdout.write(b"* Connection closed\r\n").await;
        }

        result
    }
}

/// Send HTTP request and read response on any Read+Write stream
async fn http_request_on_stream<S, W>(
    stream: &mut S,
    host: &str,
    path: &str,
    verbose: bool,
    stdout: &mut W,
) -> Result<String, &'static str>
where
    S: embedded_io_async::Read + embedded_io_async::Write,
    W: embedded_io_async::Write,
{
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: akuma-curl/1.0\r\n\r\n",
        path, host
    );

    if verbose {
        let _ = stdout.write(b"\r\n[Request]\r\n").await;
        for line in request.lines() {
            let msg = format!("> {}\r\n", line);
            let _ = stdout.write(msg.as_bytes()).await;
        }
    }

    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|_| "Write failed")?;

    // Flush to ensure data is sent through TLS layer
    stream.flush().await.map_err(|_| "Flush failed")?;

    let mut response_data = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => response_data.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }

    let response_str = String::from_utf8(response_data).map_err(|_| "Invalid UTF-8 response")?;

    // Split headers and body
    if let Some(body_start) = response_str.find("\r\n\r\n") {
        let headers = &response_str[..body_start];
        let body = &response_str[body_start + 4..];

        if verbose {
            let _ = stdout.write(b"\r\n[Response Headers]\r\n").await;
            for line in headers.lines() {
                let msg = format!("< {}\r\n", line);
                let _ = stdout.write(msg.as_bytes()).await;
            }
        }

        Ok(body.to_string())
    } else {
        Ok(response_str)
    }
}

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

use crate::async_fs;
use crate::async_net;
use crate::dns;
use crate::shell::{Command, ShellContext, ShellError, VecWriter};
use crate::tls::{TLS_RECORD_SIZE, TlsOptions, TlsStream};

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
    /// Output file path (-o, --output)
    output: Option<&'a str>,
}

impl<'a> CurlOpts<'a> {
    /// Parse command-line arguments
    fn parse(args: &'a str) -> Self {
        let mut opts = CurlOpts::default();
        let mut args_iter = args.split_whitespace().peekable();

        while let Some(arg) = args_iter.next() {
            match arg {
                "-k" | "--insecure" => opts.insecure = true,
                "-v" | "--verbose" => opts.verbose = true,
                "-o" | "--output" => {
                    // Consume the next argument as the output filename
                    opts.output = args_iter.next();
                }
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
        "curl [-k] [-v] [-o file] <url>"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        _ctx: &'a mut ShellContext,
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
                let _ = stdout
                    .write(b"Usage: curl [-k] [-v] [-o file] <url>\r\n")
                    .await;
                let _ = stdout.write(b"\r\n").await;
                let _ = stdout.write(b"Options:\r\n").await;
                let _ = stdout
                    .write(b"  -k, --insecure  Skip certificate verification\r\n")
                    .await;
                let _ = stdout
                    .write(b"  -v, --verbose   Show detailed connection info\r\n")
                    .await;
                let _ = stdout
                    .write(b"  -o, --output    Write output to file (required for binary)\r\n")
                    .await;
                let _ = stdout.write(b"\r\n").await;
                let _ = stdout.write(b"Examples:\r\n").await;
                let _ = stdout.write(b"  curl http://10.0.2.2:8080/\r\n").await;
                let _ = stdout.write(b"  curl https://example.com/\r\n").await;
                let _ = stdout
                    .write(b"  curl -o binary.elf http://10.0.2.2:8000/file.bin\r\n")
                    .await;
                let _ = stdout
                    .write(b"  curl -k https://self-signed.example.com/\r\n")
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

            // Use raw HTTP fetch to get bytes + headers
            match http_get_raw(url, tls_opts, stdout).await {
                Ok(response) => {
                    let is_binary = response.is_binary_content();

                    if let Some(output_path) = opts.output {
                        // Write to file (works for both binary and text)
                        match async_fs::write_file(output_path, &response.body).await {
                            Ok(()) => {
                                let msg = format!(
                                    "Saved {} bytes to {}\r\n",
                                    response.body.len(),
                                    output_path
                                );
                                let _ = stdout.write(msg.as_bytes()).await;
                            }
                            Err(e) => {
                                let msg = format!("Error writing file: {:?}\r\n", e);
                                let _ = stdout.write(msg.as_bytes()).await;
                            }
                        }
                    } else if is_binary {
                        // Binary content without -o flag
                        let content_type = response
                            .content_type
                            .as_deref()
                            .unwrap_or("application/octet-stream");
                        let msg = format!(
                            "Binary content detected ({}), {} bytes\r\n\
                             Use -o <filename> to save to file\r\n",
                            content_type,
                            response.body.len()
                        );
                        let _ = stdout.write(msg.as_bytes()).await;
                    } else {
                        // Text content - display to stdout
                        match String::from_utf8(response.body) {
                            Ok(text) => {
                                for line in text.split('\n') {
                                    let _ = stdout.write(line.as_bytes()).await;
                                    let _ = stdout.write(b"\r\n").await;
                                }
                            }
                            Err(_) => {
                                let _ = stdout
                                    .write(b"Error: Response contains invalid UTF-8\r\n")
                                    .await;
                                let _ = stdout
                                    .write(b"Use -o <filename> to save as binary\r\n")
                                    .await;
                            }
                        }
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
        _ctx: &'a mut ShellContext,
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
// Pkg Command
// ============================================================================

/// Pkg command - package manager for userspace binaries
pub struct PkgCommand;

impl Command for PkgCommand {
    fn name(&self) -> &'static str {
        "pkg"
    }
    fn description(&self) -> &'static str {
        "Package manager for userspace binaries"
    }
    fn usage(&self) -> &'static str {
        "pkg install <package>"
    }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        _ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let args_str = match core::str::from_utf8(args) {
                Ok(s) => s.trim(),
                Err(_) => {
                    let _ = stdout.write(b"Error: Invalid arguments\r\n").await;
                    return Ok(());
                }
            };

            let mut parts = args_str.split_whitespace();
            let subcommand = parts.next();
            let package = parts.next();

            match (subcommand, package) {
                (Some("install"), Some(pkg_name)) => {
                    // Build URL: http://10.0.2.2:8000/target/aarch64-unknown-none/release/<package>
                    let url = format!(
                        "http://10.0.2.2:8000/target/aarch64-unknown-none/release/{}",
                        pkg_name
                    );
                    let output_path = format!("/bin/{}", pkg_name);

                    let msg = format!("Installing {} from {}...\r\n", pkg_name, url);
                    let _ = stdout.write(msg.as_bytes()).await;

                    // Use http_get_raw to download the binary
                    let tls_opts = TlsOptions::default();
                    match http_get_raw(&url, tls_opts, stdout).await {
                        Ok(response) => {
                            if response.body.is_empty() {
                                let _ = stdout.write(b"Error: Empty response\r\n").await;
                                return Ok(());
                            }

                            // Write to /bin/<package>
                            match async_fs::write_file(&output_path, &response.body).await {
                                Ok(()) => {
                                    let msg = format!(
                                        "Installed {} ({} bytes) to {}\r\n",
                                        pkg_name,
                                        response.body.len(),
                                        output_path
                                    );
                                    let _ = stdout.write(msg.as_bytes()).await;
                                }
                                Err(e) => {
                                    let msg = format!("Error writing file: {:?}\r\n", e);
                                    let _ = stdout.write(msg.as_bytes()).await;
                                }
                            }
                        }
                        Err(e) => {
                            let msg = format!("Error downloading: {}\r\n", e);
                            let _ = stdout.write(msg.as_bytes()).await;
                        }
                    }
                }
                (Some("install"), None) => {
                    let _ = stdout.write(b"Error: No package name specified\r\n").await;
                    let _ = stdout.write(b"Usage: pkg install <package>\r\n").await;
                }
                (Some(cmd), _) => {
                    let msg = format!("Unknown subcommand: {}\r\n", cmd);
                    let _ = stdout.write(msg.as_bytes()).await;
                    let _ = stdout.write(b"Available: install\r\n").await;
                }
                (None, _) => {
                    let _ = stdout.write(b"Usage: pkg install <package>\r\n").await;
                    let _ = stdout.write(b"\r\n").await;
                    let _ = stdout.write(b"Examples:\r\n").await;
                    let _ = stdout.write(b"  pkg install stdcheck\r\n").await;
                    let _ = stdout.write(b"  pkg install echo2\r\n").await;
                }
            }

            Ok(())
        })
    }
}

/// Static instance
pub static PKG_CMD: PkgCommand = PkgCommand;

// ============================================================================
// HTTP Helper
// ============================================================================

/// Perform an HTTP GET request (public for legacy API)
/// Returns text content as a String. For binary content, use `http_get_raw`.
#[allow(dead_code)]
pub async fn http_get_legacy(url: &str) -> Result<String, &'static str> {
    let response = http_get_raw(url, TlsOptions::default(), &mut DummyWriter).await?;
    String::from_utf8(response.body).map_err(|_| "Invalid UTF-8 response")
}

/// Dummy writer for legacy API that doesn't need verbose output
#[allow(dead_code)]
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

// ============================================================================
// HTTP Response with Headers
// ============================================================================

/// HTTP response with parsed headers and raw body
pub struct HttpResponse {
    /// HTTP status code (kept for future use)
    #[allow(dead_code)]
    pub status_code: u16,
    /// Content-Type header value
    pub content_type: Option<String>,
    /// Raw response body as bytes
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Check if the content appears to be binary based on Content-Type header
    pub fn is_binary_content(&self) -> bool {
        match &self.content_type {
            Some(ct) => {
                let ct_lower = ct.to_lowercase();
                // Text types that should be displayed
                if ct_lower.starts_with("text/")
                    || ct_lower.contains("json")
                    || ct_lower.contains("xml")
                    || ct_lower.contains("javascript")
                    || ct_lower.contains("html")
                {
                    false
                } else {
                    // Binary types: application/octet-stream, image/*, audio/*, video/*, etc.
                    true
                }
            }
            // No Content-Type header - try to detect from body
            None => {
                // Check if body contains non-UTF8 or binary markers
                // Simple heuristic: check first 512 bytes for null bytes or high ratio of non-ASCII
                let check_len = core::cmp::min(512, self.body.len());
                let sample = &self.body[..check_len];

                // Count null bytes and non-printable characters
                let mut null_count = 0;
                let mut non_printable = 0;

                for &byte in sample {
                    if byte == 0 {
                        null_count += 1;
                    } else if byte < 32 && byte != b'\n' && byte != b'\r' && byte != b'\t' {
                        non_printable += 1;
                    }
                }

                // If we have null bytes or >10% non-printable, it's likely binary
                null_count > 0 || (check_len > 0 && non_printable * 10 > check_len)
            }
        }
    }
}

/// Perform an HTTP/HTTPS GET request and return raw response
///
/// Returns the raw bytes and parsed headers for binary/text detection.
async fn http_get_raw<W: embedded_io_async::Write>(
    url: &str,
    tls_opts: TlsOptions,
    stdout: &mut W,
) -> Result<HttpResponse, &'static str> {
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

        let mut tls =
            match TlsStream::connect_with_options(socket, host, &mut tls_rx, &mut tls_tx, tls_opts)
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

        let result = http_request_raw(&mut tls, host, &path, verbose, stdout).await;

        // Try to close TLS gracefully (ignore errors)
        let _ = tls.close().await;

        if verbose {
            let _ = stdout.write(b"* Connection closed\r\n").await;
        }

        result
    } else {
        // Plain HTTP
        let result = http_request_raw(&mut socket, host, &path, verbose, stdout).await;
        socket.close();

        if verbose {
            let _ = stdout.write(b"* Connection closed\r\n").await;
        }

        result
    }
}

/// Send HTTP request and read raw response on any Read+Write stream
async fn http_request_raw<S, W>(
    stream: &mut S,
    host: &str,
    path: &str,
    verbose: bool,
    stdout: &mut W,
) -> Result<HttpResponse, &'static str>
where
    S: embedded_io_async::Read + embedded_io_async::Write,
    W: embedded_io_async::Write,
{
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: akuma-curl/1.0\r\n\r\n",
        path, host
    );

    if verbose {
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

    // Find header/body separator
    let header_end = find_header_end(&response_data).ok_or("Invalid HTTP response")?;

    // Parse headers (as UTF-8, headers should always be ASCII)
    let headers_bytes = &response_data[..header_end];
    let headers_str = core::str::from_utf8(headers_bytes).map_err(|_| "Invalid headers")?;

    // Parse status code
    let status_code = parse_status_code(headers_str).unwrap_or(0);

    // Parse Content-Type header
    let content_type = parse_content_type(headers_str);

    if verbose {
        for line in headers_str.lines() {
            let msg = format!("< {}\r\n", line);
            let _ = stdout.write(msg.as_bytes()).await;
        }
    }

    // Body starts after \r\n\r\n (4 bytes after header_end position)
    let body = response_data[header_end + 4..].to_vec();

    Ok(HttpResponse {
        status_code,
        content_type,
        body,
    })
}

/// Find the position of the header/body separator (\r\n\r\n)
fn find_header_end(data: &[u8]) -> Option<usize> {
    for i in 0..data.len().saturating_sub(3) {
        if &data[i..i + 4] == b"\r\n\r\n" {
            return Some(i);
        }
    }
    None
}

/// Parse HTTP status code from response
fn parse_status_code(headers: &str) -> Option<u16> {
    // HTTP/1.1 200 OK
    let first_line = headers.lines().next()?;
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() >= 2 {
        parts[1].parse().ok()
    } else {
        None
    }
}

/// Parse Content-Type header value
fn parse_content_type(headers: &str) -> Option<String> {
    for line in headers.lines() {
        let lower = line.to_lowercase();
        if lower.starts_with("content-type:") {
            let value = line[13..].trim();
            // Strip charset and other parameters
            let main_type = value.split(';').next().unwrap_or(value).trim();
            return Some(main_type.to_string());
        }
    }
    None
}

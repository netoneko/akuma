//! Network Commands
//!
//! Commands for network operations: curl, nslookup, pkg

use alloc::boxed::Box;
use alloc::vec;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use embedded_io_async::Write;

use crate::async_fs::{AsyncFile, OpenMode};
use crate::shell::{execute_external_streaming, Command, ShellContext, ShellError, VecWriter};
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

/// Perform an HTTP GET request and return the HTTP status code and response body.
async fn http_get(url: &ParsedUrl) -> Result<(u16, Vec<u8>), &'static str> {
    // Resolve hostname
    let ip = smoltcp_net::dns_query(&url.host)
        .map_err(|_| "DNS resolution failed")?;

    // Connect
    let (mut stream, handle) = smoltcp_net::tcp_connect(
        smoltcp::wire::IpAddress::Ipv4(ip),
        url.port,
    ).await.map_err(|_| "TCP connection failed")?;

    // Send HTTP/1.0 GET request
    let request = format!(
        "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: akuma/1.0\r\nConnection: close\r\n\r\n",
        url.path, url.host
    );
    let _ = stream.write(request.as_bytes()).await.map_err(|_| "Send failed")?;
    let _ = stream.flush().await;

    // Read the full response
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        smoltcp_net::poll();
        match embedded_io_async::Read::read(&mut stream, &mut buf).await {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(_) => {
                smoltcp_net::socket_close(handle);
                return Err("TCP read error");
            }
        }
    }
    smoltcp_net::socket_close(handle);

    if response.is_empty() {
        return Err("Empty response");
    }

    // Parse HTTP response: find end of headers (\r\n\r\n)
    let pos = match response.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(p) => p,
        None => return Err("Malformed HTTP response"),
    };

    let header_bytes = &response[..pos];
    let body = response[pos + 4..].to_vec();

    // Parse status code from "HTTP/1.1 200 OK"
    let status_code = if let Ok(header_str) = core::str::from_utf8(header_bytes) {
        header_str
            .lines()
            .next()
            .and_then(|status_line| status_line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .unwrap_or(0)
    } else {
        0
    };

    Ok((status_code, body))
}

/// Stream an HTTP GET response body directly to a file on disk, avoiding
/// holding the entire payload in memory. Returns the HTTP status code and
/// the number of bytes written.
async fn http_get_streaming_to_file<W: embedded_io_async::Write>(
    url: &ParsedUrl,
    dest_path: &str,
    stdout: &mut W,
) -> Result<(u16, usize), &'static str> {
    let ip = smoltcp_net::dns_query(&url.host)
        .map_err(|_| "DNS resolution failed")?;

    let (mut stream, handle) = smoltcp_net::tcp_connect(
        smoltcp::wire::IpAddress::Ipv4(ip),
        url.port,
    ).await.map_err(|_| "TCP connection failed")?;

    let request = format!(
        "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: akuma/1.0\r\nConnection: close\r\n\r\n",
        url.path, url.host
    );
    let _ = stream.write(request.as_bytes()).await.map_err(|_| "Send failed")?;
    let _ = stream.flush().await;

    // Read headers into a small buffer. We accumulate until we see \r\n\r\n,
    // then everything after that boundary is the start of the body.
    let mut header_buf = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end;

    loop {
        smoltcp_net::poll();
        match embedded_io_async::Read::read(&mut stream, &mut buf).await {
            Ok(0) => {
                smoltcp_net::socket_close(handle);
                return Err("Connection closed before headers complete");
            }
            Ok(n) => {
                header_buf.extend_from_slice(&buf[..n]);
                if let Some(pos) = header_buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    header_end = pos;
                    break;
                }
                if header_buf.len() > 16384 {
                    smoltcp_net::socket_close(handle);
                    return Err("HTTP headers too large");
                }
            }
            Err(_) => {
                smoltcp_net::socket_close(handle);
                return Err("TCP read error during headers");
            }
        }
    }

    let status_code = if let Ok(hdr) = core::str::from_utf8(&header_buf[..header_end]) {
        hdr.lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse::<u16>().ok())
            .unwrap_or(0)
    } else {
        0
    };

    // Open destination file for writing
    let mut file = AsyncFile::open(dest_path, OpenMode::Write)
        .await
        .map_err(|_| "Failed to open destination file")?;

    let mut total_written: usize = 0;

    // Write the leftover body bytes that came in with the header read
    let leftover = &header_buf[header_end + 4..];
    if !leftover.is_empty() {
        let _ = file.write(leftover).await.map_err(|_| "File write error")?;
        total_written += leftover.len();
    }
    drop(header_buf);

    // Stream remaining body directly to disk
    loop {
        smoltcp_net::poll();
        match embedded_io_async::Read::read(&mut stream, &mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let _ = file.write(&buf[..n]).await.map_err(|_| {
                    smoltcp_net::socket_close(handle);
                    "File write error"
                })?;
                total_written += n;
                if total_written % (512 * 1024) < n {
                    let msg = format!("pkg: ... {} KB downloaded\r\n", total_written / 1024);
                    let _ = stdout.write(msg.as_bytes()).await;
                }
            }
            Err(_) => {
                smoltcp_net::socket_close(handle);
                return Err("TCP read error during body");
            }
        }
    }
    smoltcp_net::socket_close(handle);
    file.close().await;

    Ok((status_code, total_written))
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
                Ok((status, body)) => {
                    let msg = format!("HTTP Status: {}\r\n", status);
                    let _ = stdout.write(msg.as_bytes()).await;
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

const PKG_SERVER: &str = "http://10.0.2.2:8000";

pub struct PkgCommand;

impl PkgCommand {
    async fn download_file_w<W: embedded_io_async::Write>(
        &self,
        path: &str,
        stdout: &mut W,
    ) -> Result<(u16, Vec<u8>), ShellError> {
        let url_str = format!("{}{}", PKG_SERVER, path);
        let url = parse_url(&url_str).ok_or(ShellError::ExecutionFailed("Invalid URL"))?;

        let msg = format!("pkg: downloading {}...\r\n", url_str);
        let _ = stdout.write(msg.as_bytes()).await;

        http_get(&url).await.map_err(|e| {
            let _ = stdout.write_all(format!("pkg: download error: {}\r\n", e).as_bytes());
            ShellError::ExecutionFailed("Download failed")
        })
    }

    async fn try_install_binary_w<W: embedded_io_async::Write>(
        &self,
        package: &str,
        stdout: &mut W,
    ) -> Result<bool, ShellError> {
        let bin_path = format!("/bin/{}", package);
        match self.download_file_w(&bin_path, stdout).await {
            Ok((200, body)) => {
                if body.is_empty() {
                    let _ = stdout.write(b"pkg: warning: downloaded binary is empty.\r\n").await;
                    return Ok(false);
                }
                
                let _ = crate::async_fs::write_file(&bin_path, &body).await;
                let msg = format!("pkg: installed {} to {}\r\n", package, bin_path);
                let _ = stdout.write(msg.as_bytes()).await;
                Ok(true)
            }
            Ok((404, _)) => Ok(false),
            Ok((status, _)) => {
                let msg = format!("pkg: failed to download binary (status: {})\r\n", status);
                let _ = stdout.write(msg.as_bytes()).await;
                Err(ShellError::ExecutionFailed("Binary download failed"))
            }
            Err(e) => Err(e),
        }
    }

    async fn try_install_archive_w<W: embedded_io_async::Write>(
        &self,
        package: &str,
        streaming: bool,
        stdout: &mut W,
        ctx: &mut ShellContext,
    ) -> Result<bool, ShellError> {
        let archive_path_gz = format!("/archives/{}.tar.gz", package);
        let archive_path_tar = format!("/archives/{}.tar", package);
        
        let extensions = [".tar.gz", ".tar"];
        let paths = [archive_path_gz.as_str(), archive_path_tar.as_str()];

        for i in 0..2 {
            let path = paths[i];
            let ext = extensions[i];
            let tmp_path = format!("/tmp/{}{}", package, ext);

            if streaming {
                let url_str = format!("{}{}", PKG_SERVER, path);
                let url = match parse_url(&url_str) {
                    Some(u) => u,
                    None => continue,
                };

                let msg = format!("pkg: streaming download {}...\r\n", url_str);
                let _ = stdout.write(msg.as_bytes()).await;

                match http_get_streaming_to_file(&url, &tmp_path, stdout).await {
                    Ok((200, size)) => {
                        if size == 0 {
                            let _ = crate::async_fs::remove_file(&tmp_path).await;
                            continue;
                        }
                        let msg = format!("pkg: downloaded {} KB to {}\r\n", size / 1024, tmp_path);
                        let _ = stdout.write(msg.as_bytes()).await;
                        let success = self.extract_and_cleanup_w(&tmp_path, stdout, ctx).await?;
                        return Ok(success);
                    }
                    Ok((404, _)) => {
                        let _ = crate::async_fs::remove_file(&tmp_path).await;
                        continue;
                    }
                    Ok((status, _)) => {
                        let _ = crate::async_fs::remove_file(&tmp_path).await;
                        let msg = format!("pkg: failed to download archive (status: {})\r\n", status);
                        let _ = stdout.write(msg.as_bytes()).await;
                        return Err(ShellError::ExecutionFailed("Archive download failed"));
                    }
                    Err(e) => {
                        let _ = crate::async_fs::remove_file(&tmp_path).await;
                        let msg = format!("pkg: streaming download error: {}\r\n", e);
                        let _ = stdout.write(msg.as_bytes()).await;
                        return Err(ShellError::ExecutionFailed("Archive download failed"));
                    }
                }
            } else {
                match self.download_file_w(path, stdout).await {
                    Ok((200, body)) => {
                        if body.is_empty() {
                            continue;
                        }
                        
                        let _ = crate::async_fs::write_file(&tmp_path, &body).await;
                        let success = self.extract_and_cleanup_w(&tmp_path, stdout, ctx).await?;
                        return Ok(success);
                    }
                    Ok((404, _)) => continue,
                    Ok((status, _)) => {
                        let msg = format!("pkg: failed to download archive (status: {})\r\n", status);
                        let _ = stdout.write(msg.as_bytes()).await;
                        return Err(ShellError::ExecutionFailed("Archive download failed"));
                    }
                    Err(_) => return Err(ShellError::ExecutionFailed("Archive download failed")),
                }
            }
        }
        Ok(false)
    }

    async fn extract_and_cleanup_w<W: embedded_io_async::Write>(
        &self,
        archive_path: &str,
        stdout: &mut W,
        ctx: &mut ShellContext,
    ) -> Result<bool, ShellError> {
        if !crate::async_fs::exists("/bin/tar").await {
            let _ = stdout.write(b"pkg: 'tar' command not found. Please 'pkg install tar' first.\r\n").await;
            let _ = crate::async_fs::remove_file(archive_path).await;
            return Ok(false);
        }

        let mut args = vec!["-xvf", archive_path, "-C", "/"];
        if archive_path.ends_with(".gz") {
            args[0] = "-xzvf";
        }
        
        let msg = format!("pkg: extracting {} to root...\r\n", archive_path);
        let _ = stdout.write(msg.as_bytes()).await;

        let result = execute_external_streaming("/bin/tar", Some(&args), Some(b""), Some(ctx.cwd()), stdout).await;
        
        let _ = crate::async_fs::remove_file(archive_path).await;
        
        if result.is_ok() {
            let _ = stdout.write(b"pkg: extraction complete.\r\n").await;
            Ok(true)
        } else {
            let _ = stdout.write(b"pkg: extraction failed.\r\n").await;
            Err(ShellError::ExecutionFailed("Extraction failed"))
        }
    }

    /// Install packages with streaming output to any writer.
    pub async fn install_streaming<W: embedded_io_async::Write>(
        &self,
        packages: &str,
        stdout: &mut W,
        ctx: &mut ShellContext,
    ) -> Result<(), ShellError> {
        let streaming = packages.split_whitespace().any(|a| a == "--streaming");
        for package in packages.split_whitespace() {
            if package.starts_with("--") {
                continue;
            }
            self.install_package_w(package, streaming, stdout, ctx).await?;
        }
        Ok(())
    }

    async fn install_package_w<W: embedded_io_async::Write>(
        &self,
        package: &str,
        streaming: bool,
        stdout: &mut W,
        ctx: &mut ShellContext,
    ) -> Result<(), ShellError> {
        if package.is_empty() {
            return Ok(());
        }

        let _ = crate::async_fs::create_dir("/bin").await;
        let _ = crate::async_fs::create_dir("/tmp").await;

        if self.try_install_binary_w(package, stdout).await? {
            return Ok(());
        }

        if self.try_install_archive_w(package, streaming, stdout, ctx).await? {
            return Ok(());
        }

        let msg = format!("pkg: unable to find package '{}'\r\n", package);
        let _ = stdout.write(msg.as_bytes()).await;

        Ok(())
    }
}

impl Command for PkgCommand {
    fn name(&self) -> &'static str { "pkg" }
    fn description(&self) -> &'static str { "Package manager" }
    fn usage(&self) -> &'static str { "pkg install [--streaming] <package1> [package2] ..." }

    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        _stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
        ctx: &'a mut ShellContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
        Box::pin(async move {
            let args_str = core::str::from_utf8(args).unwrap_or("").trim();

            if let Some(rest) = args_str.strip_prefix("install") {
                let packages = rest.trim();
                if packages.is_empty() {
                    let _ = stdout.write(b"Usage: pkg install <package>\r\n").await;
                    return Ok(());
                }

                self.install_streaming(packages, stdout, ctx).await?;
            } else {
                let _ = stdout.write(b"Usage: pkg install <package1> [package2] ...\r\n").await;
            }

            Ok(())
        })
    }
}

pub static PKG_CMD: PkgCommand = PkgCommand;

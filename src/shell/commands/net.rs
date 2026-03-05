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
use akuma_net::http::{self, ParsedUrl, parse_url};
use akuma_net::smoltcp_net;

/// Adapter that wraps `AsyncFile` to implement `embedded_io_async::Write`.
struct AsyncFileWriter(AsyncFile);

impl embedded_io_async::ErrorType for AsyncFileWriter {
    type Error = embedded_io_async::ErrorKind;
}

impl embedded_io_async::Write for AsyncFileWriter {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.0.write(buf).await.map_err(|_| embedded_io_async::ErrorKind::Other)
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Stream an HTTP GET response body directly to a file on disk via
/// `akuma_net::http::http_get_streaming`. Returns status code and bytes written.
async fn http_get_streaming_to_file(
    url: &ParsedUrl,
    dest_path: &str,
) -> Result<(u16, usize), &'static str> {
    let file = AsyncFile::open(dest_path, OpenMode::Write)
        .await
        .map_err(|_| "Failed to open destination file")?;
    let mut writer = AsyncFileWriter(file);

    let result = http::http_get_streaming(url, false, &mut writer, |_| {}).await;
    writer.0.close().await;
    result
}

// ============================================================================
// Curl Command
// ============================================================================

pub struct CurlCommand;

impl Command for CurlCommand {
    fn name(&self) -> &'static str { "curl" }
    fn description(&self) -> &'static str { "HTTP/HTTPS GET request" }
    fn usage(&self) -> &'static str { "curl [-k] [-L] [-v] <url>" }

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
                let _ = stdout.write(b"Usage: curl [-k] [-L] [-v] <url>\r\n").await;
                return Ok(());
            }

            let mut insecure = false;
            let mut follow_redirects = false;
            let mut verbose = false;
            let mut url_str = None;

            for token in args_str.split_whitespace() {
                match token {
                    "-k" | "--insecure" => insecure = true,
                    "-L" | "--location" => follow_redirects = true,
                    "-v" | "--verbose" => verbose = true,
                    "-Lv" | "-vL" => { follow_redirects = true; verbose = true; }
                    "-Lk" | "-kL" => { follow_redirects = true; insecure = true; }
                    "-kv" | "-vk" => { insecure = true; verbose = true; }
                    "-Lkv" | "-Lvk" | "-kLv" | "-kvL" | "-vLk" | "-vkL" => {
                        follow_redirects = true; insecure = true; verbose = true;
                    }
                    _ if !token.starts_with('-') => url_str = Some(token),
                    _ => {}
                }
            }

            let raw_url = match url_str {
                Some(u) => u,
                None => {
                    let _ = stdout.write(b"Usage: curl [-k] [-L] [-v] <url>\r\n").await;
                    return Ok(());
                }
            };

            let max_redirects = if follow_redirects { 10 } else { 0 };
            let mut current_url_string = String::from(raw_url);

            for redirect_count in 0..=max_redirects {
                let url = match parse_url(&current_url_string) {
                    Some(u) => u,
                    None => {
                        let _ = stdout.write(b"Error: Invalid URL\r\n").await;
                        return Ok(());
                    }
                };

                if verbose {
                    let scheme = if url.is_https { "https" } else { "http" };
                    let msg = format!("* Connecting to {}:{} ({})\r\n", url.host, url.port, scheme);
                    let _ = stdout.write(msg.as_bytes()).await;
                }

                match http::http_get(&url, insecure).await {
                    Ok(resp) => {
                        if verbose {
                            let msg = format!("< HTTP/1.0 {}\r\n", resp.status);
                            let _ = stdout.write(msg.as_bytes()).await;
                        }

                        if follow_redirects && (301..=308).contains(&resp.status) {
                            if redirect_count >= max_redirects {
                                let _ = stdout.write(b"Error: Too many redirects\r\n").await;
                                return Ok(());
                            }
                            if let Some(location) = resp.location() {
                                if verbose {
                                    let msg = format!("* Redirecting to {}\r\n", location);
                                    let _ = stdout.write(msg.as_bytes()).await;
                                }
                                current_url_string = String::from(location);
                                continue;
                            }
                        }

                        let _ = stdout.write(&resp.body).await;
                        if !resp.body.ends_with(b"\n") {
                            let _ = stdout.write(b"\r\n").await;
                        }
                    }
                    Err(e) => {
                        let msg = format!("Error: {}\r\n", e);
                        let _ = stdout.write(msg.as_bytes()).await;
                    }
                }
                break;
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

        let resp = http::http_get(&url, false).await.map_err(|e| {
            let _ = stdout.write_all(format!("pkg: download error: {}\r\n", e).as_bytes());
            ShellError::ExecutionFailed("Download failed")
        })?;
        Ok((resp.status, resp.body))
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

                match http_get_streaming_to_file(&url, &tmp_path).await {
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

        let result = execute_external_streaming("/bin/tar", Some(&args), None, Some(b""), Some(ctx.cwd()), stdout).await;
        
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

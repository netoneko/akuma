//! Simple HTTP/HTTPS client.
//!
//! Provides `http_get` that works over both plain TCP and TLS,
//! returning status code, headers, and body.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::smoltcp_net;
use crate::tls::{TlsOptions, TlsStream, TLS_RECORD_SIZE};

/// Parsed URL with scheme awareness.
pub struct ParsedUrl {
    pub host: String,
    pub port: u16,
    pub path: String,
    pub is_https: bool,
}

/// Parse a URL string into components.
#[allow(clippy::option_if_let_else)]
#[must_use]
pub fn parse_url(url: &str) -> Option<ParsedUrl> {
    let (is_https, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        (false, url)
    };

    let default_port = if is_https { 443 } else { 80 };

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
            (host_port, default_port)
        }
    } else {
        (host_port, default_port)
    };

    if host.is_empty() {
        return None;
    }

    Some(ParsedUrl {
        host: String::from(host),
        port,
        path: String::from(path),
        is_https,
    })
}

/// HTTP response from `http_get`.
pub struct HttpResponse {
    pub status: u16,
    pub headers: String,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Extract the `Location` header value (for redirects).
    #[must_use] 
    pub fn location(&self) -> Option<&str> {
        for line in self.headers.lines() {
            if let Some(value) = line.strip_prefix("Location: ")
                .or_else(|| line.strip_prefix("location: "))
            {
                return Some(value.trim());
            }
        }
        None
    }
}

/// Perform an HTTP or HTTPS GET, returning status, headers, and body.
pub async fn http_get(url: &ParsedUrl, insecure: bool) -> Result<HttpResponse, &'static str> {
    let ip = smoltcp_net::dns_query(&url.host)
        .map_err(|_| "DNS resolution failed")?;

    let (mut stream, handle) = smoltcp_net::tcp_connect(
        smoltcp::wire::IpAddress::Ipv4(ip),
        url.port,
    )
    .await
    .map_err(|_| "TCP connection failed")?;

    let request = format!(
        "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: akuma/1.0\r\nConnection: close\r\n\r\n",
        url.path, url.host
    );

    let result = if url.is_https {
        let mut read_buf = vec![0u8; TLS_RECORD_SIZE + 1024];
        let mut write_buf = vec![0u8; TLS_RECORD_SIZE + 1024];
        let tls_opts = if insecure {
            TlsOptions::new().insecure()
        } else {
            TlsOptions::new()
        };
        let mut tls = TlsStream::connect_with_options(
            stream,
            &url.host,
            &mut read_buf,
            &mut write_buf,
            tls_opts,
        )
        .await
        .map_err(|e| {
            log::warn!("[HTTP] TLS handshake failed: {e:?}");
            "TLS handshake failed"
        })?;

        log::info!("[HTTP] TLS handshake complete, sending request");

        let _ = embedded_io_async::Write::write(&mut tls, request.as_bytes())
            .await
            .map_err(|e| {
                log::warn!("[HTTP] TLS send failed: {e:?}");
                "TLS send failed"
            })?;
        let _ = embedded_io_async::Write::flush(&mut tls).await;

        log::info!("[HTTP] Request sent, reading response...");

        let r = read_http_response(&mut tls).await;
        if let Err(e) = &r {
            log::warn!("[HTTP] TLS read result: {e}");
        }
        let _ = tls.close().await;
        r
    } else {
        let _ = embedded_io_async::Write::write(&mut stream, request.as_bytes())
            .await
            .map_err(|_| "Send failed")?;
        let _ = embedded_io_async::Write::flush(&mut stream).await;

        read_http_response(&mut stream).await
    };

    smoltcp_net::socket_close(handle);
    result
}

/// Stream an HTTP/HTTPS GET response body to a writer.
///
/// Connects, sends the request, parses headers, then streams the body
/// directly to `body_writer`. Returns the status code and total bytes written.
/// Calls `on_progress(bytes_so_far)` after each chunk so the caller can
/// display download progress.
pub async fn http_get_streaming<W: embedded_io_async::Write>(
    url: &ParsedUrl,
    _insecure: bool,
    body_writer: &mut W,
    on_progress: impl Fn(usize),
) -> Result<(u16, usize), &'static str> {
    let ip = smoltcp_net::dns_query(&url.host)
        .map_err(|_| "DNS resolution failed")?;

    let (mut stream, handle) = smoltcp_net::tcp_connect(
        smoltcp::wire::IpAddress::Ipv4(ip),
        url.port,
    )
    .await
    .map_err(|_| "TCP connection failed")?;

    let request = format!(
        "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: akuma/1.0\r\nConnection: close\r\n\r\n",
        url.path, url.host
    );
    let _ = embedded_io_async::Write::write(&mut stream, request.as_bytes())
        .await
        .map_err(|_| "Send failed")?;
    let _ = embedded_io_async::Write::flush(&mut stream).await;

    let (status, total) =
        stream_response_body(&mut stream, handle, body_writer, &on_progress).await?;
    smoltcp_net::socket_close(handle);
    Ok((status, total))
}

/// Read headers, then stream body chunks to `dest`. Returns (status, total bytes).
async fn stream_response_body<R: embedded_io_async::Read, W: embedded_io_async::Write>(
    reader: &mut R,
    handle: smoltcp_net::SocketHandle,
    dest: &mut W,
    on_progress: &impl Fn(usize),
) -> Result<(u16, usize), &'static str> {
    let mut header_buf = Vec::new();
    let mut buf = [0u8; 4096];

    let header_end = loop {
        smoltcp_net::poll();
        match embedded_io_async::Read::read(reader, &mut buf).await {
            Ok(0) => {
                smoltcp_net::socket_close(handle);
                return Err("Connection closed before headers complete");
            }
            Ok(n) => {
                header_buf.extend_from_slice(&buf[..n]);
                if let Some(pos) = header_buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos;
                }
                if header_buf.len() > 16384 {
                    smoltcp_net::socket_close(handle);
                    return Err("HTTP headers too large");
                }
            }
            Err(_) => {
                smoltcp_net::socket_close(handle);
                return Err("Read error during headers");
            }
        }
    };

    let status = core::str::from_utf8(&header_buf[..header_end])
        .ok()
        .and_then(|h| h.lines().next())
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);

    let mut total: usize = 0;

    let leftover = &header_buf[header_end + 4..];
    if !leftover.is_empty() {
        let _ = dest.write(leftover).await.map_err(|_| "Write error")?;
        total += leftover.len();
        on_progress(total);
    }
    drop(header_buf);

    loop {
        smoltcp_net::poll();
        match embedded_io_async::Read::read(reader, &mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let _ = dest.write(&buf[..n]).await.map_err(|_| "Write error")?;
                total += n;
                on_progress(total);
            }
            Err(_) => {
                if total > 0 {
                    break;
                }
                return Err("Read error during body");
            }
        }
    }

    Ok((status, total))
}

async fn read_http_response<R: embedded_io_async::Read>(
    reader: &mut R,
) -> Result<HttpResponse, &'static str> {
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        smoltcp_net::poll();
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(_) => {
                // TLS connections may signal EOF via error after close_notify.
                // If we already have data, treat as normal EOF.
                if !response.is_empty() {
                    break;
                }
                return Err("Read error");
            }
        }
    }

    if response.is_empty() {
        return Err("Empty response");
    }

    let Some(pos) = response.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Err("Malformed HTTP response");
    };

    let headers: String = core::str::from_utf8(&response[..pos])
        .unwrap_or("")
        .into();
    let body = response[pos + 4..].to_vec();

    let status = core::str::from_utf8(&response[..pos])
        .ok()
        .and_then(|h| h.lines().next())
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);

    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

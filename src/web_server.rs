//! HTTP Web Server using edge-http
//!
//! Serves static files from the /public directory on port 8080.
//! Returns 404 for files not found.

use alloc::format;
use alloc::string::String;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use edge_http::Method;
use edge_http::io::server::Connection;
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_time::Duration;
use embedded_io_async::Write;

use crate::async_fs;

// ============================================================================
// Constants
// ============================================================================

const HTTP_PORT: u16 = 8080;
const MAX_CONNECTIONS: usize = 4;
const TCP_RX_BUFFER_SIZE: usize = 2048;
const TCP_TX_BUFFER_SIZE: usize = 4096;
const HTTP_BUF_SIZE: usize = 1024;

// ============================================================================
// Buffer Pool
// ============================================================================

struct BufferPool {
    rx_buffers: [UnsafeCell<[u8; TCP_RX_BUFFER_SIZE]>; MAX_CONNECTIONS + 1],
    tx_buffers: [UnsafeCell<[u8; TCP_TX_BUFFER_SIZE]>; MAX_CONNECTIONS + 1],
    http_buffers: [UnsafeCell<[u8; HTTP_BUF_SIZE]>; MAX_CONNECTIONS + 1],
    in_use: [AtomicBool; MAX_CONNECTIONS + 1],
}

unsafe impl Sync for BufferPool {}

impl BufferPool {
    const fn new() -> Self {
        const RX_INIT: UnsafeCell<[u8; TCP_RX_BUFFER_SIZE]> =
            UnsafeCell::new([0u8; TCP_RX_BUFFER_SIZE]);
        const TX_INIT: UnsafeCell<[u8; TCP_TX_BUFFER_SIZE]> =
            UnsafeCell::new([0u8; TCP_TX_BUFFER_SIZE]);
        const HTTP_INIT: UnsafeCell<[u8; HTTP_BUF_SIZE]> = UnsafeCell::new([0u8; HTTP_BUF_SIZE]);
        const IN_USE_INIT: AtomicBool = AtomicBool::new(false);

        Self {
            rx_buffers: [RX_INIT; MAX_CONNECTIONS + 1],
            tx_buffers: [TX_INIT; MAX_CONNECTIONS + 1],
            http_buffers: [HTTP_INIT; MAX_CONNECTIONS + 1],
            in_use: [IN_USE_INIT; MAX_CONNECTIONS + 1],
        }
    }

    fn alloc(&self) -> Option<usize> {
        for i in 0..=MAX_CONNECTIONS {
            if self.in_use[i]
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(i);
            }
        }
        None
    }

    fn free(&self, slot: usize) {
        if slot <= MAX_CONNECTIONS {
            self.in_use[slot].store(false, Ordering::Release);
        }
    }

    unsafe fn get_buffers(
        &self,
        slot: usize,
    ) -> (&'static mut [u8], &'static mut [u8], &'static mut [u8]) {
        debug_assert!(slot <= MAX_CONNECTIONS);
        unsafe {
            let rx = &mut *self.rx_buffers[slot].get();
            let tx = &mut *self.tx_buffers[slot].get();
            let http = &mut *self.http_buffers[slot].get();
            (rx, tx, http)
        }
    }
}

static BUFFER_POOL: BufferPool = BufferPool::new();

// ============================================================================
// HTTP Server
// ============================================================================

/// Run the HTTP web server on port 8080
pub async fn run(stack: Stack<'static>) {

    loop {
        // Allocate a buffer slot
        let slot = match BUFFER_POOL.alloc() {
            Some(s) => s,
            None => {
                embassy_time::Timer::after(Duration::from_millis(10)).await;
                continue;
            }
        };

        // Get buffers for this connection
        let (rx_buf, tx_buf, http_buf) = unsafe { BUFFER_POOL.get_buffers(slot) };

        let mut socket = TcpSocket::new(stack, rx_buf, tx_buf);
        socket.set_timeout(Some(Duration::from_secs(30)));

        // Wait for a connection
        match socket.accept(HTTP_PORT).await {
            Ok(()) => {
                handle_connection(&mut socket, http_buf).await;
                let _ = socket.flush().await;
                socket.close();
                // Wait a bit for the close to propagate through loopback
                embassy_time::Timer::after(Duration::from_millis(10)).await;
            }
            Err(_) => {}
        }

        // Free the buffer slot
        BUFFER_POOL.free(slot);
    }
}

/// Handle a single HTTP connection using edge-http
async fn handle_connection(socket: &mut TcpSocket<'_>, http_buf: &mut [u8]) {
    // Parse the HTTP request using edge-http
    let mut conn: Connection<'_, _, 16> = match Connection::new(http_buf, socket).await {
        Ok(conn) => conn,
        Err(_) => return,
    };

    // Get request headers
    let headers = match conn.headers() {
        Ok(h) => h,
        Err(_) => return,
    };

    let method = headers.method;
    let path = if headers.path.is_empty() {
        "/"
    } else {
        headers.path
    };

    // Only support GET and HEAD
    match method {
        Method::Get | Method::Head => {}
        _ => {
            let _ = send_error(&mut conn, 405, "Method Not Allowed").await;
            return;
        }
    }

    let is_head = matches!(method, Method::Head);

    // Normalize path - map to /public directory
    let fs_path = if path == "/" {
        String::from("/public/index.html")
    } else {
        format!("/public{}", path)
    };

    // Security: prevent directory traversal
    if path.contains("..") {
        let _ = send_error(&mut conn, 403, "Forbidden").await;
        return;
    }

    // Check if filesystem is initialized
    if !crate::fs::is_initialized() {
        let _ = send_error(&mut conn, 503, "Service Unavailable").await;
        return;
    }

    // Try to read the file
    match async_fs::read_file(&fs_path).await {
        Ok(content) => {
            let content_type = get_content_type(&fs_path);
            let _ = send_file(&mut conn, &content, content_type, is_head).await;
        }
        Err(_) => {
            let _ = send_error(&mut conn, 404, "Not Found").await;
        }
    }
}

/// Get content type based on file extension
fn get_content_type(path: &str) -> &'static str {
    if path.ends_with(".html") || path.ends_with(".htm") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".json") {
        "application/json; charset=utf-8"
    } else if path.ends_with(".txt") {
        "text/plain; charset=utf-8"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        "image/jpeg"
    } else if path.ends_with(".gif") {
        "image/gif"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else {
        "application/octet-stream"
    }
}

/// Send a file response using edge-http
async fn send_file<T: embedded_io_async::Read + Write>(
    conn: &mut Connection<'_, T, 16>,
    content: &[u8],
    content_type: &str,
    head_only: bool,
) -> Result<(), ()> {
    let content_len_str = format!("{}", content.len());
    let headers = [
        ("Content-Type", content_type),
        ("Content-Length", &content_len_str),
        ("Connection", "close"),
    ];

    conn.initiate_response(200, Some("OK"), &headers)
        .await
        .map_err(|_| ())?;

    if !head_only {
        conn.write_all(content).await.map_err(|_| ())?;
    }

    conn.complete().await.map_err(|_| ())?;

    Ok(())
}

/// Send an error response using edge-http
async fn send_error<T: embedded_io_async::Read + Write>(
    conn: &mut Connection<'_, T, 16>,
    code: u16,
    message: &str,
) -> Result<(), ()> {
    let body = format!(
        "<!DOCTYPE html>\n<html><head><title>{} {}</title></head>\n\
         <body><h1>{} {}</h1></body></html>\n",
        code, message, code, message
    );

    let content_len_str = format!("{}", body.len());
    let headers = [
        ("Content-Type", "text/html; charset=utf-8"),
        ("Content-Length", &content_len_str),
        ("Connection", "close"),
    ];

    conn.initiate_response(code, Some(message), &headers)
        .await
        .map_err(|_| ())?;

    conn.write_all(body.as_bytes()).await.map_err(|_| ())?;
    conn.complete().await.map_err(|_| ())?;

    Ok(())
}


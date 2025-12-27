//! Netcat Server - Async Accept Loop
//!
//! Manages the netcat/telnet server accept loop that listens for connections
//! and handles them asynchronously.

use embassy_net::Stack;
use embassy_time::{Duration, Timer};

use crate::akuma::AKUMA_79;
use crate::async_net::{TcpListener, TcpStream};
use crate::console;

// ============================================================================
// Constants
// ============================================================================

const TELNET_PORT: u16 = 23;

// ============================================================================
// Connection Handler
// ============================================================================

async fn handle_connection(mut stream: TcpStream) {
    log("[Netcat] Client connected\n");

    // Send welcome message
    if stream
        .write_all(b"*** Welcome to Akuma Telnet Server ***\r\n")
        .await
        .is_err()
    {
        return;
    }
    if stream
        .write_all(b"Type something and press Enter (echo server)\r\n")
        .await
        .is_err()
    {
        return;
    }
    if stream
        .write_all(b"Type 'quit' to disconnect\r\n\r\n")
        .await
        .is_err()
    {
        return;
    }

    let mut buf = [0u8; 512];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => {
                log("[Netcat] Connection closed by peer\n");
                break;
            }
            Ok(len) => {
                let data = &buf[..len];

                // Check for quit command
                if len >= 4 && (data.starts_with(b"quit") || data.starts_with(b"exit")) {
                    let _ = stream.write_all(b"Goodbye!\r\n").await;
                    log("[Netcat] Client disconnected (quit)\n");
                    break;
                }

                // Check for cat command
                if len >= 3 && data.starts_with(b"cat") {
                    let _ = stream.write_all(AKUMA_79).await;
                    continue;
                }

                // Echo back with prefix
                let _ = stream.write_all(b"echo: ").await;
                let _ = stream.write_all(data).await;
            }
            Err(_) => {
                log("[Netcat] Read error\n");
                break;
            }
        }
    }

    stream.close();
    log("[Netcat] Connection ended\n");
}

// ============================================================================
// Netcat Server Accept Loop
// ============================================================================

/// Run the netcat server accept loop
pub async fn run(stack: Stack<'static>) {
    log("[Netcat Server] Starting telnet server on port 23...\n");
    log("[Netcat Server] Connect with: telnet localhost 2323\n");

    let listener = TcpListener::new(stack, TELNET_PORT);

    loop {
        match listener.accept().await {
            Ok(stream) => {
                log("[Netcat Server] Accepted new connection\n");
                handle_connection(stream).await;
                log("[Netcat Server] Connection handled, listening again...\n");
            }
            Err(e) => {
                log(&alloc::format!(
                    "[Netcat Server] Accept error: {:?}, retrying...\n",
                    e
                ));
                Timer::after(Duration::from_millis(100)).await;
            }
        }
    }
}

// ============================================================================
// Logging
// ============================================================================

fn log(msg: &str) {
    console::print(msg);
}


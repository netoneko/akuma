//! Netcat-style TCP echo server
//!
//! Simple telnet-like server that echoes back data and supports basic commands.

use crate::akuma::AKUMA_79;
use crate::console;
use crate::network;
use crate::threading;

use smoltcp::socket::tcp::{Socket as TcpSocket, State as TcpState};

// ============================================================================
// Constants
// ============================================================================

const LISTEN_PORT: u16 = 23; // Telnet port, forwarded from host 2323

// ============================================================================
// Logging Helper
// ============================================================================

fn log(msg: &str) {
    console::print(msg);
}

// ============================================================================
// TCP Handler
// ============================================================================

/// Poll the network stack and handle netcat socket
/// Returns true if there was activity
pub fn poll() -> bool {
    network::with_netstack(|stack| {
        let socket = stack.sockets.get_mut::<TcpSocket>(stack.tcp_handle);

        // Check for new connection
        if socket.state() == TcpState::Established && !stack.was_connected {
            stack.was_connected = true;
            network::increment_connections();
            log("\n[Net] *** Client connected! ***\n");
            log("[Net] Type something and press Enter (echo server)\n");
            log("[Net] Type 'quit' to disconnect\n\n");
            return true;
        } else if socket.state() == TcpState::Listen || socket.state() == TcpState::Closed {
            stack.was_connected = false;
        }

        // Echo any received data back
        if socket.can_recv() {
            let mut buf = [0u8; 512];
            match socket.recv_slice(&mut buf) {
                Ok(len) if len > 0 => {
                    network::add_bytes_rx(len as u64);

                    // Check for 'quit' command
                    let data = &buf[..len];
                    if len >= 4 && (data.starts_with(b"quit") || data.starts_with(b"exit")) {
                        let _ = socket.send_slice(b"Goodbye!\r\n");
                        socket.close();
                        log("[Net] Client disconnected (quit)\n");
                        return true;
                    }

                    if len >= 3 && data.starts_with(b"cat") {
                        let _ = socket.send_slice(AKUMA_79);
                        network::add_bytes_tx(AKUMA_79.len() as u64);
                        return true;
                    }

                    // Echo back with prefix
                    if socket.can_send() {
                        let _ = socket.send_slice(b"echo: ");
                        let _ = socket.send_slice(&buf[..len]);
                        network::add_bytes_tx(6 + len as u64);
                    }
                    return true;
                }
                _ => {}
            }
        }

        // Re-listen if socket closed
        if socket.state() == TcpState::Closed {
            let _ = socket.listen(LISTEN_PORT);
        }

        false
    })
    .unwrap_or(false)
}

// ============================================================================
// Public API
// ============================================================================

/// Thread entry point for netcat server
pub fn netcat_server_entry() -> ! {
    log("[Net] Netcat server thread started\n");

    loop {
        // Poll the network interface first
        network::poll_interface();
        // Then handle our socket
        poll();
        threading::yield_now();
    }
}
